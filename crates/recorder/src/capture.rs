//! The capture role (`docs/architecture.md`): one `Recording` per live capture, owning its spool and its per-track
//! pipelines. Driven by the controller over the control socket (`control.rs`); reports status over Redis when
//! Redis is configured.

use crate::config::Config;
use crate::pipeline::{PipelineEvent, TrackPipeline, TrackPipelineOptions};
use crate::ports::PortAllocator;
use crate::registry::Registry;
use crate::spool::{free_bytes, Manifest, Spool, TrackManifest};
use crate::storage::Storage;
use crate::wire::{
    now_ms, AllocateTrack, AllocateTrackResponse, CaptureStart, FailReason, RecordingStatus,
    StatusEvent, StopReason, TrackKind,
};
use anyhow::{anyhow, bail, Context, Result};
use socketioxide::extract::SocketRef;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

/// Shared state of the capture role.
pub struct Capture {
    pub config: Config,
    pub ports: PortAllocator,
    /// Discovery, status and the compose wake-up; `None` without Redis.
    pub registry: Option<AsyncMutex<Registry>>,
    /// The bucket; `None` keeps finished recordings in the spool.
    pub storage: Option<Storage>,
    /// The tokio runtime, for work started from a GStreamer thread (pipeline events arrive there; a bare
    /// `tokio::spawn` from such a thread panics, and a panic inside a GStreamer callback aborts the process).
    pub rt: tokio::runtime::Handle,
    recordings: Mutex<HashMap<String, Arc<Recording>>>,
}

pub struct Recording {
    pub id: String,
    pub start: CaptureStart,
    pub started: Instant,
    spool: Mutex<Spool>,
    tracks: Mutex<HashMap<String, Track>>,
    finished: Mutex<Vec<TrackManifest>>,
    /// Video tracks with a keyframe chase running (one per track); the flag asks it to start over from the
    /// current frame count, for a gap that lands while the chase from connect is still running.
    chasing: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Held (read) while a released track is being finished, so `stop` waits for those files before it writes
    /// the manifest: a peer leaving as the room closes must not lose its track from the manifest.
    releasing: tokio::sync::RwLock<()>,
    stopped: AtomicBool,
    /// The controller socket that owns this capture; lost socket ⇒ orphan grace (`docs/architecture.md`).
    pub owner: Mutex<Option<SocketRef>>,
    capture: Weak<Capture>,
}

struct Track {
    peer_id: String,
    kind: TrackKind,
    codec: String,
    clock_rate: u32,
    file: PathBuf,
    rtp_port: u16,
    rtcp_port: u16,
    pipeline: Option<TrackPipeline>,
}

impl Capture {
    pub fn new(config: Config, registry: Option<Registry>, storage: Option<Storage>) -> Arc<Self> {
        Arc::new(Self {
            ports: PortAllocator::new(config.rtc_port_min, config.rtc_port_max),
            registry: registry.map(AsyncMutex::new),
            storage,
            rt: tokio::runtime::Handle::current(),
            recordings: Mutex::new(HashMap::new()),
            config,
        })
    }

    /// 0..1: how full this recorder is, by port pairs in use. A controller picks the lowest.
    pub fn load(&self) -> f64 {
        let capacity = ((self.config.rtc_port_max - self.config.rtc_port_min) / 2).max(1) as f64;
        self.ports.in_use() as f64 / capacity
    }

    pub fn live_count(&self) -> usize {
        self.recordings.lock().unwrap().len()
    }

    pub fn recording(&self, id: &str) -> Result<Arc<Recording>> {
        self.recordings
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("recording {id} not found"))
    }

    pub fn recordings_owned_by(&self, socket: &SocketRef) -> Vec<Arc<Recording>> {
        self.recordings
            .lock()
            .unwrap()
            .values()
            .filter(|r| {
                r.owner
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|s| s.id == socket.id)
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// `capture.start`: refuse without spool space, open the spool, arm the length cap.
    pub async fn start(self: &Arc<Self>, req: CaptureStart, owner: SocketRef) -> Result<()> {
        let free = free_bytes(&self.config.spool_dir).unwrap_or(u64::MAX);
        if free < self.config.min_free_bytes {
            bail!("disk_full");
        }
        if self
            .recordings
            .lock()
            .unwrap()
            .contains_key(&req.recording_id)
        {
            bail!("recording {} already running here", req.recording_id);
        }
        let mut spool = Spool::create(&self.config.spool_dir, &req.recording_id)?;
        spool.event(without_nulls(serde_json::json!({
            "type": "recording.started", "at": now_ms(),
            "recordingId": req.recording_id, "prefix": req.storage_prefix(),
            "scopeId": req.scope_id, "callId": req.call_id, "sessionId": req.session_id,
            "accountId": req.account_id, "metadata": req.metadata,
            "layoutPolicy": req.policy.name, "policyVersion": req.policy.version,
            "recorderId": self.config.id,
        })))?;
        for p in &req.peers {
            spool.event(without_nulls(serde_json::json!({ "type": "peer.joined", "at": now_ms(), "peerId": p.peer_id,
                "userId": p.user_id, "name": p.name, "picture": p.picture, "userType": p.user_type })))?;
        }

        let recording = Arc::new(Recording {
            id: req.recording_id.clone(),
            started: Instant::now(),
            spool: Mutex::new(spool),
            tracks: Mutex::new(HashMap::new()),
            finished: Mutex::new(Vec::new()),
            chasing: Mutex::new(HashMap::new()),
            releasing: tokio::sync::RwLock::new(()),
            stopped: AtomicBool::new(false),
            owner: Mutex::new(Some(owner)),
            capture: Arc::downgrade(self),
            start: req,
        });
        self.recordings
            .lock()
            .unwrap()
            .insert(recording.id.clone(), recording.clone());
        self.publish(
            &recording.id,
            RecordingStatus::Recording,
            None,
            None,
            None,
            None,
        )
        .await;

        // A recording never runs longer than the cap.
        let cap = Duration::from_millis(self.config.max_recording_ms);
        let weak = Arc::downgrade(&recording);
        tokio::spawn(async move {
            tokio::time::sleep(cap).await;
            if let Some(r) = weak.upgrade() {
                r.end_from_recorder(StopReason::MaxDuration).await;
            }
        });
        tracing::info!(recording = %recording.id, "capture started");
        Ok(())
    }

    async fn publish(
        &self,
        id: &str,
        status: RecordingStatus,
        stop: Option<StopReason>,
        fail: Option<FailReason>,
        detail: Option<String>,
        artifacts: Option<Vec<String>>,
    ) {
        let event = StatusEvent {
            recording_id: id.to_string(),
            status,
            at: now_ms(),
            stop_reason: stop,
            fail_reason: fail,
            fail_detail: detail,
            artifacts,
        };
        let Some(registry) = &self.registry else {
            tracing::info!(recording = %id, status = ?status, "status (no redis to publish to)");
            return;
        };
        if let Err(e) = registry.lock().await.publish_status(&event).await {
            tracing::error!(recording = %id, error = %e, "status publish failed");
        }
    }

    fn remove(&self, id: &str) {
        self.recordings.lock().unwrap().remove(id);
    }

    /// Spool → bucket, then `captured` and the compose wake-up; on failure `failed upload_failed` and the spool
    /// stays in place for an operator to re-queue. Without a bucket the spool is the destination: `captured`
    /// names the local files and the compose worker finds the folder by its manifest. Shared by the live stop
    /// and the orphan sweep.
    pub async fn upload_spool(
        &self,
        id: &str,
        dir: &std::path::Path,
        prefix: &str,
        reason: StopReason,
    ) {
        let Some(storage) = &self.storage else {
            let artifacts = local_artifacts(dir);
            tracing::info!(recording = %id, files = artifacts.len(), dir = %dir.display(), "captured; kept in the spool (no bucket)");
            self.publish(
                id,
                RecordingStatus::Captured,
                Some(reason),
                None,
                None,
                Some(artifacts),
            )
            .await;
            self.wake_compose(id, prefix).await;
            return;
        };
        let started = Instant::now();
        match storage.upload_dir(dir, prefix).await {
            Ok(files) => {
                let bytes: u64 = files.iter().map(|f| f.bytes).sum();
                tracing::info!(recording = %id, files = files.len(), bytes, ms = started.elapsed().as_millis() as u64,
                    bucket = %storage.bucket, %prefix, "uploaded");
                let artifacts = files
                    .into_iter()
                    .map(|f| f.key.trim_start_matches(&format!("{prefix}/")).to_string())
                    .collect();
                self.publish(
                    id,
                    RecordingStatus::Captured,
                    Some(reason),
                    None,
                    None,
                    Some(artifacts),
                )
                .await;
                self.wake_compose(id, prefix).await;
            }
            Err(e) => {
                tracing::error!(recording = %id, error = format!("{e:#}"), "upload failed; spool kept");
                self.publish(
                    id,
                    RecordingStatus::Failed,
                    Some(reason),
                    Some(FailReason::UploadFailed),
                    Some(format!("{e:#}")),
                    None,
                )
                .await;
            }
        }
    }

    async fn wake_compose(&self, id: &str, prefix: &str) {
        let Some(registry) = &self.registry else {
            return;
        };
        if let Err(e) = registry.lock().await.enqueue_compose(prefix).await {
            tracing::warn!(recording = %id, error = %e, "compose wake-up failed; the worker's scan will find it");
        }
    }
}

/// Relative names of everything in a spool folder, for a `captured` status without a bucket.
fn local_artifacts(dir: &std::path::Path) -> Vec<String> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.extension().map(|e| e != "tmp").unwrap_or(true) {
                out.push(
                    path.strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

impl Recording {
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn capture(&self) -> Result<Arc<Capture>> {
        self.capture
            .upgrade()
            .ok_or_else(|| anyhow!("capture is shutting down"))
    }

    /// `capture.allocateTrack`: reserve ports, build the pipeline, answer where the SFU must send.
    pub async fn allocate_track(
        self: &Arc<Self>,
        req: AllocateTrack,
    ) -> Result<AllocateTrackResponse> {
        if self.is_stopped() {
            bail!("recording {} is stopped", self.id);
        }
        let capture = self.capture()?;
        if self.tracks.lock().unwrap().contains_key(&req.track_id) {
            bail!("track {} already allocated", req.track_id);
        }
        let (rtp_port, rtcp_port) = capture.ports.allocate()?;
        let file = self
            .spool
            .lock()
            .unwrap()
            .track_file(&req.peer_id, &req.track_id)?;

        let weak = Arc::downgrade(self);
        let track_id = req.track_id.clone();
        let on_event: Box<dyn Fn(PipelineEvent) + Send + Sync> = Box::new(move |ev| {
            if let Some(r) = weak.upgrade() {
                r.on_pipeline_event(&track_id, ev);
            }
        });
        let bind_ip = capture.config.rtc_ip.clone();
        let jitter = capture.config.jitter_buffer_ms;
        let started = self.started;
        let file_for_pipeline = file.clone();
        let req_for_pipeline = req.clone();
        let pipeline = tokio::task::spawn_blocking(move || {
            TrackPipeline::start(TrackPipelineOptions {
                track: &req_for_pipeline,
                rtp_port,
                rtcp_port,
                bind_ip: &bind_ip,
                file: &file_for_pipeline,
                jitter_buffer_ms: jitter,
                started,
                on_event,
            })
        })
        .await
        .context("pipeline task")?;
        let pipeline = match pipeline {
            Ok(p) => p,
            Err(e) => {
                capture.ports.release(rtp_port);
                return Err(e);
            }
        };

        let codec = req
            .rtp_parameters
            .codecs
            .first()
            .map(|c| c.mime_type.clone())
            .unwrap_or_else(|| req.codec.clone());
        self.tracks.lock().unwrap().insert(
            req.track_id.clone(),
            Track {
                peer_id: req.peer_id.clone(),
                kind: req.kind,
                codec,
                clock_rate: req.clock_rate,
                file: file.clone(),
                rtp_port,
                rtcp_port,
                pipeline: Some(pipeline),
            },
        );
        let relative = self.spool.lock().unwrap().relative(&file);
        self.event(
            serde_json::json!({ "type": "track.allocated", "at": now_ms(), "trackId": req.track_id,
            "peerId": req.peer_id, "kind": req.kind, "codec": req.codec, "file": relative }),
        )?;
        Ok(AllocateTrackResponse {
            ip: capture.config.announced_ip.clone(),
            port: rtp_port,
            rtcp_port,
        })
    }

    /// `capture.trackConnected`: now we know where to send RTCP; the first RTP packet marks `track.started`.
    pub fn track_connected(
        self: &Arc<Self>,
        track_id: &str,
        consumer_id: &str,
        rtcp_ip: &str,
        rtcp_port: u16,
    ) -> Result<()> {
        let tracks = self.tracks.lock().unwrap();
        let track = tracks
            .get(track_id)
            .ok_or_else(|| anyhow!("track {track_id} not allocated"))?;
        if let Some(p) = &track.pipeline {
            p.connect_rtcp(rtcp_ip, rtcp_port);
        }
        let is_video = matches!(track.kind, TrackKind::Webcam | TrackKind::Screen);
        drop(tracks);
        if is_video {
            self.chase_keyframe(track_id);
        }
        self.event(serde_json::json!({ "type": "track.started", "at": now_ms(), "trackId": track_id, "consumerId": consumer_id }))
    }

    /// Until frames reach the file again, ask for a keyframe every second (up to 30 s). A single request is not
    /// always answered: at connect the consumer may not be flowing yet, and after a loss the browser has been
    /// seen to take ten seconds. The depayloader drops everything until the keyframe, so the file only ever
    /// holds decodable frames.
    fn chase_keyframe(self: &Arc<Self>, track_id: &str) {
        let Ok(capture) = self.capture() else {
            return;
        };
        let restart = {
            let mut chasing = self.chasing.lock().unwrap();
            if let Some(running) = chasing.get(track_id) {
                running.store(true, Ordering::SeqCst);
                return;
            }
            let flag = Arc::new(AtomicBool::new(false));
            chasing.insert(track_id.to_string(), flag.clone());
            flag
        };
        let recording = self.clone();
        let track_id = track_id.to_string();
        let frames_at = |r: &Recording, id: &str| {
            r.tracks
                .lock()
                .unwrap()
                .get(id)
                .and_then(|t| t.pipeline.as_ref())
                .map(|p| p.progress())
        };
        let moved = |base: Option<(u64, u64)>, now: Option<(u64, u64)>| match (base, now) {
            (Some(b), Some(n)) => n.0 > b.0 || n.1 > b.1,
            _ => false,
        };
        let mut baseline = frames_at(self, &track_id);
        capture.rt.spawn(async move {
            let mut attempts = 0;
            while attempts < 30 {
                recording.request_keyframe(&track_id);
                attempts += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
                let now = frames_at(&recording, &track_id);
                if restart.swap(false, Ordering::SeqCst) {
                    baseline = now;
                    attempts = 0;
                    continue;
                }
                if now.is_none() || moved(baseline, now) || recording.is_stopped() {
                    break;
                }
            }
            recording.chasing.lock().unwrap().remove(&track_id);
        });
    }

    /// Ask the controller for a keyframe on a video track (it asks the producer through the SFU).
    fn request_keyframe(&self, track_id: &str) {
        if let Some(owner) = self.owner.lock().unwrap().clone() {
            let _ = owner.emit("notification", &serde_json::json!({
                "method": "needKeyFrame", "data": { "recordingId": self.id, "trackId": track_id }
            }));
            let _ = self.event(serde_json::json!({ "type": "recorder.keyframe_requested", "at": now_ms(), "trackId": track_id }));
        }
    }

    /// `capture.releaseTrack`: finish the file and remember it for the manifest.
    pub async fn release_track(self: &Arc<Self>, track_id: &str) -> Result<()> {
        let _in_flight = self.releasing.read().await;
        let track = self.tracks.lock().unwrap().remove(track_id);
        let Some(track) = track else { return Ok(()) };
        self.finish_track(track_id, track).await;
        Ok(())
    }

    async fn finish_track(&self, track_id: &str, mut track: Track) {
        let stats = if let Some(pipeline) = track.pipeline.take() {
            let stats = pipeline.stats.clone();
            let _ =
                tokio::task::spawn_blocking(move || pipeline.stop(Duration::from_secs(3))).await;
            let s = stats.lock().unwrap();
            Some((
                s.first_rtp_ts,
                s.packets,
                s.packets_lost,
                s.bytes,
                s.first_frame_t.or(s.start_t),
                s.end_t,
                s.sender_reports.clone(),
                s.gaps.clone(),
            ))
        } else {
            None
        };
        if let Ok(capture) = self.capture() {
            capture.ports.release(track.rtp_port);
        }
        let _ = track.rtcp_port;
        let (first_rtp_ts, packets, packets_lost, bytes, start_t, end_t, sender_reports, gaps) =
            stats.unwrap_or((None, 0, 0, 0, None, 0, Vec::new(), Vec::new()));
        let file = self.spool.lock().unwrap().relative(&track.file);
        let bytes = std::fs::metadata(&track.file)
            .map(|m| m.len())
            .unwrap_or(bytes);
        self.finished.lock().unwrap().push(TrackManifest {
            track_id: track_id.to_string(),
            peer_id: track.peer_id,
            kind: track.kind,
            codec: track.codec,
            clock_rate: track.clock_rate,
            file,
            start_t: start_t.unwrap_or(0),
            end_t,
            bytes,
            first_rtp_ts: first_rtp_ts.unwrap_or(0),
            sender_reports,
            packets,
            packets_lost,
            gaps,
        });
    }

    /// `capture.event`: one event-log line from the controller; `t` is stamped here. A pause or resume also
    /// tells the track's pipeline whether silence is expected.
    pub fn event(&self, value: serde_json::Value) -> Result<()> {
        let paused = match value["type"].as_str() {
            Some("track.paused") => Some(true),
            Some("track.resumed") => Some(false),
            _ => None,
        };
        if let (Some(paused), Some(track_id)) = (paused, value["trackId"].as_str()) {
            if let Some(pipeline) = self
                .tracks
                .lock()
                .unwrap()
                .get(track_id)
                .and_then(|t| t.pipeline.as_ref())
            {
                pipeline.set_quiet(paused);
            }
        }
        self.spool.lock().unwrap().event(json_map(value))
    }

    /// `capture.stop` from the controller, or our own end: close every track, write the manifest, report.
    pub async fn stop(self: &Arc<Self>, reason: StopReason) -> Result<()> {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        tracing::info!(recording = %self.id, ?reason, "capture stopping");
        let tracks: Vec<(String, Track)> = self.tracks.lock().unwrap().drain().collect();
        // Every file gets its EOS at the same time: the tails stay in step and a stop with many tracks
        // does not take one EOS timeout per track.
        for (_, track) in &tracks {
            if let Some(pipeline) = &track.pipeline {
                pipeline.mark_stopping();
            }
        }
        futures_util::future::join_all(
            tracks
                .into_iter()
                .map(|(id, track)| async move { self.finish_track(&id, track).await }),
        )
        .await;
        // Releases that were in flight when the stop arrived land in `finished` before the manifest is built.
        drop(self.releasing.write().await);
        let stopped_at = now_ms();
        self.event(
            serde_json::json!({ "type": "recording.stopped", "at": stopped_at, "reason": reason }),
        )?;

        let manifest = Manifest {
            version: 1,
            recording_id: self.id.clone(),
            prefix: Some(self.key_prefix()),
            scope_id: self.start.scope_id.clone(),
            call_id: self.start.call_id.clone(),
            account_id: self.start.account_id.clone(),
            session_id: self.start.session_id.clone(),
            metadata: self.start.metadata.clone(),
            recorder_id: self
                .capture()
                .map(|c| c.config.id.clone())
                .unwrap_or_default(),
            started_at: self.start.started_at,
            title: self.start.title.clone(),
            started_by_name: self.start.started_by_name.clone(),
            stopped_at,
            stop_reason: reason,
            duration_ms: self.started.elapsed().as_millis() as u64,
            policy: self.start.policy.clone(),
            peers: self.start.peers.clone(),
            tracks: self.finished.lock().unwrap().clone(),
            composite: None,
            thumbnail: None,
            audio_mix: None,
        };
        self.spool.lock().unwrap().write_manifest(&manifest)?;

        // The spool is complete and durable here; the upload runs on its own so the controller gets its answer
        // at once, and `captured` goes out only when the bucket holds the manifest.
        if let Ok(capture) = self.capture() {
            capture.remove(&self.id);
            let dir = self.spool.lock().unwrap().dir.clone();
            let (id, prefix) = (self.id.clone(), self.key_prefix());
            tokio::spawn(async move { capture.upload_spool(&id, &dir, &prefix, reason).await });
        }
        Ok(())
    }

    /// The bucket prefix (`docs/spool-format.md`): explicit from `capture.start`, else derived.
    pub fn key_prefix(&self) -> String {
        self.start.storage_prefix()
    }

    /// The recorder ends the capture by itself (cap reached, disk, orphaned): tell the controller, then stop.
    pub async fn end_from_recorder(self: &Arc<Self>, reason: StopReason) {
        if self.is_stopped() {
            return;
        }
        if let Some(owner) = self.owner.lock().unwrap().clone() {
            let _ = owner.emit(
                "notification",
                &serde_json::json!({
                    "method": "captureEnded", "data": { "recordingId": self.id, "reason": reason }
                }),
            );
        }
        if let Err(e) = self.stop(reason).await {
            tracing::error!(recording = %self.id, error = %e, "stop failed");
        }
    }

    fn on_pipeline_event(self: &Arc<Self>, track_id: &str, ev: PipelineEvent) {
        match ev {
            PipelineEvent::Gap {
                from_t,
                to_t,
                lost_packets,
            } => {
                let _ = self.event(
                    serde_json::json!({ "type": "recorder.gap", "at": now_ms(), "trackId": track_id,
                    "fromT": from_t, "toT": to_t, "lostPackets": lost_packets }),
                );
                let is_video = self
                    .tracks
                    .lock()
                    .unwrap()
                    .get(track_id)
                    .map(|t| matches!(t.kind, TrackKind::Webcam | TrackKind::Screen))
                    .unwrap_or(false);
                if is_video {
                    self.chase_keyframe(track_id);
                }
            }
            PipelineEvent::Orientation { t, rotation, flip } => {
                let _ = self.event(serde_json::json!({ "type": "recorder.orientation", "at": now_ms(), "trackId": track_id,
                    "atT": t, "rotation": rotation, "flip": flip }));
            }
            PipelineEvent::Error(message) => {
                tracing::error!(recording = %self.id, track = %track_id, %message, "pipeline error");
                let _ = self.event(serde_json::json!({ "type": "recorder.track_error", "at": now_ms(), "trackId": track_id, "message": message }));
                let this = self.clone();
                let track_id = track_id.to_string();
                if let Ok(capture) = self.capture() {
                    capture.rt.spawn(async move {
                        let _ = this.release_track(&track_id).await;
                    });
                }
            }
        }
    }
}

fn json_map(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match v {
        serde_json::Value::Object(m) => m,
        other => {
            let mut m = serde_json::Map::new();
            m.insert("value".into(), other);
            m
        }
    }
}

/// An event line without the keys whose value is absent, so optional ids are simply not there.
fn without_nulls(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut m = json_map(v);
    m.retain(|_, v| !v.is_null());
    m
}
