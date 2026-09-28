//! The spool: local disk staging for one recording (`docs/spool-format.md`).
//! `<spool>/<recordingId>/{events.jsonl, manifest.json, tracks/<peerId>/<trackId>.webm}`.
//! Everything here is append-only and flushed per line, so a crash at any second leaves what came before.

use crate::wire::{PeerInfo, Policy, StopReason, TrackKind};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Spool {
    pub dir: PathBuf,
    events: File,
    started: Instant,
}

impl Spool {
    /// Create `<root>/<recordingId>/` and open `events.jsonl`. Fails if it already exists: ids are unique.
    pub fn create(root: &Path, recording_id: &str) -> Result<Self> {
        let dir = root.join(recording_id);
        if dir.exists() {
            anyhow::bail!("spool {} already exists", dir.display());
        }
        fs::create_dir_all(dir.join("tracks"))
            .with_context(|| format!("create {}", dir.display()))?;
        let events = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(dir.join("events.jsonl"))?;
        Ok(Self {
            dir,
            events,
            started: Instant::now(),
        })
    }

    /// Milliseconds since `recording.started` on this recorder's monotonic clock (the `t` of every event).
    pub fn t(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Append one event line. `fields` must carry `type` and `at`; `t` is stamped here.
    pub fn event(&mut self, mut fields: serde_json::Map<String, serde_json::Value>) -> Result<()> {
        fields.insert("t".into(), serde_json::Value::from(self.t()));
        let mut line = serde_json::to_string(&fields)?;
        line.push('\n');
        self.events.write_all(line.as_bytes())?;
        self.events.flush()?;
        Ok(())
    }

    pub fn track_file(&self, peer_id: &str, track_id: &str) -> Result<PathBuf> {
        let dir = self.dir.join("tracks").join(sanitize(peer_id));
        fs::create_dir_all(&dir)?;
        Ok(dir.join(format!("{}.webm", sanitize(track_id))))
    }

    /// Path relative to the recording folder, as stored in events and the manifest.
    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    pub fn write_manifest(&self, manifest: &Manifest) -> Result<()> {
        let tmp = self.dir.join("manifest.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(manifest)?)?;
        fs::rename(&tmp, self.dir.join("manifest.json"))?;
        Ok(())
    }
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Free bytes on the file system holding `path`.
pub fn free_bytes(path: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(path.as_os_str().as_bytes())?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("statvfs");
        }
        Ok(st.f_bavail as u64 * st.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}

/// `manifest.json`: the truth for what artifacts exist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: u32,
    pub recording_id: String,
    /// The bucket prefix this recording lives under. Manifests written before this field existed derive it
    /// from the application ids (`storage_prefix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Application identifiers, copied from `capture.start`; `meetingId` is an older name for `scopeId`.
    #[serde(default, alias = "meetingId", skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub recorder_id: String,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by_name: Option<String>,
    pub stopped_at: u64,
    pub stop_reason: StopReason,
    pub duration_ms: u64,
    pub policy: Policy,
    pub peers: Vec<PeerInfo>,
    pub tracks: Vec<TrackManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite: Option<CompositeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<ThumbnailInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_mix: Option<AudioMixInfo>,
}

impl Manifest {
    /// The bucket prefix: the stored one, else `<accountId>/<scopeId>/<recordingId>` when both ids are there,
    /// else the recording id alone.
    pub fn storage_prefix(&self) -> String {
        if let Some(p) = self.prefix.as_deref().filter(|p| !p.is_empty()) {
            return p.to_string();
        }
        match (&self.account_id, &self.scope_id) {
            (Some(a), Some(s)) if !a.is_empty() && !s.is_empty() => {
                format!("{a}/{s}/{}", self.recording_id)
            }
            _ => self.recording_id.clone(),
        }
    }
}

/// Written by compose.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompositeInfo {
    pub file: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bytes: u64,
    pub duration_ms: u64,
    pub policy: String,
    pub policy_version: u32,
    pub composed_at: u64,
    /// Track files the manifest names that were not there to compose.
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThumbnailInfo {
    pub file: String,
    pub at_t: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioMixInfo {
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackManifest {
    pub track_id: String,
    pub peer_id: String,
    pub kind: TrackKind,
    pub codec: String,
    pub clock_rate: u32,
    pub file: String,
    /// `t` of the first packet written and of the last one.
    pub start_t: u64,
    pub end_t: u64,
    pub bytes: u64,
    /// RTP timestamp of the first packet in the file (file time 0), and the Sender Report mapping that anchors it:
    /// capture NTP of `sr_rtp_ts` is `sr_ntp_s`. Compose aligns tracks from these (`docs/spool-format.md`).
    pub first_rtp_ts: u32,
    pub sender_reports: Vec<SenderReportSample>,
    pub packets: u64,
    pub packets_lost: u64,
    pub gaps: Vec<Gap>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SenderReportSample {
    /// Recorder-clock ms since recording start when the report arrived.
    pub t: u64,
    pub ntp_s: f64,
    pub rtp_ts: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gap {
    pub from_t: u64,
    pub to_t: u64,
    pub lost_packets: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_manifests_still_read_and_derive_their_prefix() {
        let old = r#"{"version":1,"recordingId":"r1","meetingId":"m1","callId":"c1","accountId":"a1",
            "sessionId":"s1","recorderId":"rec","startedAt":0,"stoppedAt":10,"stopReason":"user","durationMs":10,
            "policy":{"name":"auto","version":1},"peers":[],"tracks":[]}"#;
        let m: Manifest = serde_json::from_str(old).unwrap();
        assert_eq!(m.storage_prefix(), "a1/m1/r1");
        let bare: Manifest = serde_json::from_str(
            r#"{"version":1,"recordingId":"r2","recorderId":"rec","startedAt":0,"stoppedAt":10,
            "stopReason":"user","durationMs":10,"policy":{"name":"auto","version":1},"peers":[],"tracks":[]}"#,
        )
        .unwrap();
        assert_eq!(bare.storage_prefix(), "r2");
        let text = serde_json::to_string(&bare).unwrap();
        assert!(!text.contains("accountId"), "absent ids are not written");
    }
}
