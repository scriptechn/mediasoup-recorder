//! The compose worker (`docs/architecture.md`): every `manifest.json` without a `composite` entry is a pending
//! job. The worker scans the local spool at start and every 30 s while idle (level-triggered), and, with Redis,
//! also wakes on the list `recording-compose` (items are bucket prefixes) so it does not have to wait for the
//! scan. It composes one recording at a time at low CPU priority, uploads when there is a bucket, publishes
//! `composing` → `ready` (or `failed compose_failed`), and deletes the spool once the bucket holds everything.
//! Without a bucket the composite stays next to its tracks in the spool.
//!
//! Any number of workers may run: the bucket manifest says whether a recording is done, and a Redis claim keeps
//! two workers off the same recording at the same time. Without Redis there is one worker per spool, and the
//! spool is the truth.

use crate::capture::Capture;
use crate::compose::{compose, ComposeOptions};
use crate::config::{Config, DEFAULT_ICON_FONT};
use crate::policy::Policy;
use crate::registry::Registry;
use crate::spool::Manifest;
use crate::storage::Storage;
use crate::wire::{now_ms, FailReason, RecordingStatus, StatusEvent};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Worker {
    config: Config,
    storage: Option<Storage>,
    policy: Policy,
    options: ComposeOptions,
    /// The capture role of this process, when it runs one: compose waits for its recordings to end.
    capture: Option<Arc<Capture>>,
}

impl Worker {
    pub fn new(
        config: Config,
        storage: Option<Storage>,
        capture: Option<Arc<Capture>>,
    ) -> Result<Arc<Self>> {
        let policy = Policy::load(&config.policy_dir, "auto")?;
        let hold: Option<Arc<dyn Fn() -> bool + Send + Sync>> =
            match (&capture, config.compose.while_capturing) {
                (Some(capture), false) => {
                    let capture = capture.clone();
                    Some(Arc::new(move || capture.live_count() > 0))
                }
                _ => None,
            };
        let options = ComposeOptions {
            width: config.compose.width,
            height: config.compose.height,
            fps: config.compose.fps,
            video_kbps: config.compose.video_kbps,
            audio_kbps: config.compose.audio_kbps,
            font: config.compose.font.clone(),
            icon_font: config.compose.icon_font.clone(),
            brand: config.compose.brand.clone(),
            hold,
        };
        Ok(Arc::new(Self {
            config,
            storage,
            policy,
            options,
            capture,
        }))
    }

    /// Runs until the process ends. With Redis it holds its own connection: BLPOP would block the shared one.
    pub async fn run(self: Arc<Self>) {
        let mut registry = match &self.config.redis {
            Some(_) => loop {
                match Registry::connect_blocking(&self.config).await {
                    Ok(r) => break Some(r),
                    Err(e) => {
                        tracing::warn!(error = %e, "compose worker: redis not reachable, retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            },
            None => None,
        };
        for prefix in self.scan_spool() {
            self.job(registry.as_mut(), &prefix).await;
        }
        let mut last_scan = std::time::Instant::now();
        loop {
            // Do not take a wake-up while a capture is live here: another worker may be free to do it now.
            self.wait_for_quiet().await;
            let woken = match registry.as_mut() {
                Some(registry) => match registry.wait_compose(30).await {
                    Ok(item) => item,
                    Err(e) => {
                        tracing::warn!(error = %e, "compose wait failed; reconnecting");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        match Registry::connect_blocking(&self.config).await {
                            Ok(r) => *registry = r,
                            Err(e) => tracing::warn!(error = %e, "redis not reachable"),
                        }
                        continue;
                    }
                },
                None => {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    None
                }
            };
            match woken {
                Some(prefix) => self.job(registry.as_mut(), &prefix).await,
                None => {
                    // Quiet: every 30 s re-check the spool, in case a wake-up was lost while we were busy.
                    if last_scan.elapsed().as_secs() >= 30 {
                        last_scan = std::time::Instant::now();
                        for prefix in self.scan_spool() {
                            self.job(registry.as_mut(), &prefix).await;
                        }
                    }
                }
            }
        }
    }

    /// Prefixes of every local spool folder with a manifest and no composite.
    fn scan_spool(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.config.spool_dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let manifest = entry.path().join("manifest.json");
            if let Ok(text) = std::fs::read_to_string(&manifest) {
                if let Ok(m) = serde_json::from_str::<Manifest>(&text) {
                    if m.composite.is_none() {
                        out.push(m.storage_prefix());
                    }
                }
            }
        }
        out
    }

    /// Blocks while a capture is live on this recorder (unless composing alongside is allowed).
    async fn wait_for_quiet(&self) {
        if self.config.compose.while_capturing {
            return;
        }
        let Some(capture) = &self.capture else { return };
        let mut said = false;
        while capture.live_count() > 0 {
            if !said {
                tracing::info!(
                    live = capture.live_count(),
                    "compose waits: a capture is live on this recorder"
                );
                said = true;
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn job(&self, mut registry: Option<&mut Registry>, prefix: &str) {
        let prefix = prefix.trim_matches('/');
        if prefix.is_empty() {
            tracing::warn!("compose wake-up with an empty prefix; ignored");
            return;
        }
        let recording_id = prefix.rsplit('/').next().unwrap_or(prefix).to_string();
        let dir = self.config.spool_dir.join(&recording_id);
        self.wait_for_quiet().await;
        // Another worker may have finished it (the wake-up went to one worker, the spool scan here saw the folder):
        // then the only work left is to drop the local copy.
        if let Some(storage) = &self.storage {
            match storage.is_composed(prefix).await {
                Ok(true) => {
                    if dir.is_dir() {
                        tracing::info!(recording = %recording_id, "already composed elsewhere; dropping local spool");
                        if let Err(e) = std::fs::remove_dir_all(&dir) {
                            tracing::warn!(recording = %recording_id, error = %e, "spool cleanup failed");
                        }
                    }
                    return;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(recording = %recording_id, error = format!("{e:#}"), "cannot read the bucket manifest; will retry");
                    return;
                }
            }
        }
        // Two workers can hold the same job (one from the list, one from its spool scan): the claim picks one.
        if let Some(registry) = registry.as_deref_mut() {
            let ttl = (self.config.max_recording_ms / 1000).max(3600);
            match registry.claim_compose(&recording_id, ttl).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!(recording = %recording_id, "another worker is composing it");
                    return;
                }
                Err(e) => {
                    tracing::warn!(recording = %recording_id, error = %e, "compose claim failed; will retry");
                    return;
                }
            }
        }
        tracing::info!(recording = %recording_id, %prefix, "compose job");
        publish(
            registry.as_deref_mut(),
            &recording_id,
            RecordingStatus::Composing,
            None,
            None,
            None,
        )
        .await;
        match self.compose_and_upload(prefix, &dir).await {
            Ok(artifacts) => {
                publish(
                    registry.as_deref_mut(),
                    &recording_id,
                    RecordingStatus::Ready,
                    None,
                    None,
                    Some(artifacts),
                )
                .await;
                // With a bucket the spool is a staging copy; without one it is the recording.
                if self.storage.is_some() {
                    if let Err(e) = std::fs::remove_dir_all(&dir) {
                        tracing::warn!(recording = %recording_id, error = %e, "spool cleanup failed");
                    }
                } else {
                    tracing::info!(recording = %recording_id, dir = %dir.display(), "ready in the spool");
                }
            }
            Err(e) => {
                tracing::error!(recording = %recording_id, error = format!("{e:#}"), "compose failed");
                publish(
                    registry.as_deref_mut(),
                    &recording_id,
                    RecordingStatus::Failed,
                    Some(FailReason::ComposeFailed),
                    Some(format!("{e:#}")),
                    None,
                )
                .await;
            }
        }
        if let Some(registry) = registry {
            if let Err(e) = registry.release_compose(&recording_id).await {
                tracing::warn!(recording = %recording_id, error = %e, "compose claim release failed (it expires)");
            }
        }
    }

    async fn compose_and_upload(&self, prefix: &str, dir: &Path) -> Result<Vec<String>> {
        if !dir.join("manifest.json").is_file() {
            let Some(storage) = &self.storage else {
                anyhow::bail!(
                    "spool {} has no manifest and there is no bucket to fetch it from",
                    dir.display()
                );
            };
            tracing::info!(%prefix, "spool not local; downloading");
            storage.download_dir(prefix, dir).await?;
        }
        let already: Manifest =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json"))?)?;
        if already.composite.is_some() {
            return Ok(artifact_names(&already));
        }
        let result = {
            let dir = dir.to_path_buf();
            let policy = self.policy.clone();
            let options = self.options.clone();
            let nice = self.config.compose.nice;
            // Its own thread, niced: GStreamer's streaming threads and x264's workers inherit the priority.
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("compose".into())
                .spawn(move || {
                    #[cfg(unix)]
                    unsafe {
                        libc::nice(nice);
                    }
                    let _ = tx.send(compose(&dir, &policy, &options));
                })
                .context("compose thread")?;
            rx.await.map_err(|_| anyhow!("compose thread died"))??
        };
        let manifest = result.manifest;
        let tmp = dir.join("manifest.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
        std::fs::rename(&tmp, dir.join("manifest.json"))?;

        if let Some(storage) = &self.storage {
            let mut files = vec![result.composite, result.thumbnail];
            files.extend(result.audio_mix);
            files.push(dir.join("manifest.json"));
            for file in &files {
                let rel = file
                    .strip_prefix(dir)
                    .unwrap_or(file)
                    .to_string_lossy()
                    .replace('\\', "/");
                storage
                    .upload_file(&format!("{prefix}/{rel}"), file)
                    .await?;
            }
        }
        Ok(artifact_names(&manifest))
    }
}

fn artifact_names(m: &Manifest) -> Vec<String> {
    let mut v: Vec<String> = m.tracks.iter().map(|t| t.file.clone()).collect();
    v.push("events.jsonl".into());
    v.push("manifest.json".into());
    if let Some(c) = &m.composite {
        v.push(c.file.clone());
    }
    if let Some(t) = &m.thumbnail {
        v.push(t.file.clone());
    }
    if let Some(a) = &m.audio_mix {
        v.push(a.file.clone());
    }
    v
}

async fn publish(
    registry: Option<&mut Registry>,
    id: &str,
    status: RecordingStatus,
    fail: Option<FailReason>,
    detail: Option<String>,
    artifacts: Option<Vec<String>>,
) {
    let Some(registry) = registry else {
        tracing::info!(recording = %id, status = ?status, "status (no redis to publish to)");
        return;
    };
    let event = StatusEvent {
        recording_id: id.to_string(),
        status,
        at: now_ms(),
        stop_reason: None,
        fail_reason: fail,
        fail_detail: detail,
        artifacts,
    };
    if let Err(e) = registry.publish_status(&event).await {
        tracing::error!(recording = %id, error = %e, "status publish failed");
    }
}

/// `recorder compose-local <dir>`: compose a spool folder in place with no Redis or bucket. The composite
/// settings come from the same `RECORDER_COMPOSE_*` variables the service reads, with the same defaults.
pub fn compose_local(dir: &Path, policy_dir: &Path, font: &Path) -> Result<PathBuf> {
    let policy = Policy::load(policy_dir, "auto")?;
    let env_or = |name: &str, default: u32| -> u32 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let options = ComposeOptions {
        width: env_or("RECORDER_COMPOSE_WIDTH", 1280),
        height: env_or("RECORDER_COMPOSE_HEIGHT", 720),
        fps: env_or("RECORDER_COMPOSE_FPS", 30),
        video_kbps: env_or("RECORDER_COMPOSE_VIDEO_KBPS", 2500),
        audio_kbps: env_or("RECORDER_COMPOSE_AUDIO_KBPS", 128),
        font: font.to_path_buf(),
        icon_font: PathBuf::from(
            std::env::var("RECORDER_COMPOSE_ICON_FONT")
                .unwrap_or_else(|_| DEFAULT_ICON_FONT.to_string()),
        ),
        brand: std::env::var("RECORDER_COMPOSE_BRAND").unwrap_or_default(),
        hold: None,
    };
    let result = compose(dir, &policy, &options)?;
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&result.manifest)?,
    )?;
    Ok(result.composite)
}
