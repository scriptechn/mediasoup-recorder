//! Interrupted captures (`docs/architecture.md`): a recorder that died mid-meeting leaves
//! `<spool>/<recordingId>/` with an event log and track files but no manifest. On start, before the compose
//! worker runs, every such folder is finalised from what is on disk (a manifest with `stopReason:
//! recorder_lost`, the track list from `track.allocated`, times from the event log), uploaded, and announced as
//! `captured`, so whoever waits on the status moves on and the composite still gets made.

use crate::capture::Capture;
use crate::spool::{Manifest, TrackManifest};
use crate::wire::{now_ms, PeerInfo, Policy, StopReason, TrackKind};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

pub async fn sweep(capture: &Arc<Capture>) {
    let Ok(entries) = std::fs::read_dir(&capture.config.spool_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || dir.join("manifest.json").exists() || !dir.join("events.jsonl").exists()
        {
            continue;
        }
        match finalise(&dir, &capture.config.id) {
            Ok(manifest) => {
                tracing::warn!(recording = %manifest.recording_id, tracks = manifest.tracks.len(),
                    duration_ms = manifest.duration_ms, "orphaned capture finalised from disk");
                let prefix = manifest.storage_prefix();
                capture
                    .upload_spool(
                        &manifest.recording_id,
                        &dir,
                        &prefix,
                        StopReason::RecorderLost,
                    )
                    .await;
            }
            Err(e) => {
                tracing::error!(dir = %dir.display(), error = format!("{e:#}"), "orphaned capture could not be finalised; left in place");
            }
        }
    }
}

/// Build and write the manifest of an interrupted capture from its event log and files.
pub fn finalise(dir: &Path, recorder_id: &str) -> Result<Manifest> {
    let text = std::fs::read_to_string(dir.join("events.jsonl"))?;
    let events: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let started = events
        .iter()
        .find(|e| e["type"] == "recording.started")
        .ok_or_else(|| anyhow!("no recording.started event"))?;
    let field = |v: &Value, k: &str| -> Option<String> { v[k].as_str().map(str::to_string) };
    let recording_id = field(started, "recordingId")
        .ok_or_else(|| anyhow!("recording.started without recordingId"))?;
    let started_at = started["at"].as_u64().unwrap_or_else(now_ms);
    // The event log only moves when something happens; the track files were written until the recorder died
    // (unbuffered sink), so their modification time is the last moment media arrived.
    let mut last_t = events
        .iter()
        .filter_map(|e| e["t"].as_u64())
        .max()
        .unwrap_or(0);
    let file_end_t = |file: &str| -> Option<u64> {
        let modified = std::fs::metadata(dir.join(file)).ok()?.modified().ok()?;
        let ms = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        Some(ms.saturating_sub(started_at))
    };
    for e in events.iter().filter(|e| e["type"] == "track.allocated") {
        if let Some(t) = e["file"].as_str().and_then(file_end_t) {
            last_t = last_t.max(t);
        }
    }

    let mut peers: BTreeMap<String, PeerInfo> = BTreeMap::new();
    for e in events.iter().filter(|e| e["type"] == "peer.joined") {
        if let Some(id) = e["peerId"].as_str() {
            peers.entry(id.to_string()).or_insert(PeerInfo {
                peer_id: id.to_string(),
                user_id: e["userId"].as_str().unwrap_or("").to_string(),
                name: e["name"].as_str().map(str::to_string),
                picture: e["picture"].as_str().map(str::to_string),
                user_type: e["userType"].as_str().map(str::to_string),
            });
        }
    }

    let mut tracks = Vec::new();
    for e in events.iter().filter(|e| e["type"] == "track.allocated") {
        let (Some(track_id), Some(peer_id), Some(file)) = (
            e["trackId"].as_str(),
            e["peerId"].as_str(),
            e["file"].as_str(),
        ) else {
            continue;
        };
        let bytes = std::fs::metadata(dir.join(file))
            .map(|m| m.len())
            .unwrap_or(0);
        if bytes == 0 {
            continue; // nothing arrived before the recorder died
        }
        let kind = match e["kind"].as_str() {
            Some("mic") => TrackKind::Mic,
            Some("screen") => TrackKind::Screen,
            _ => TrackKind::Webcam,
        };
        let started_t = events
            .iter()
            .find(|x| x["type"] == "track.started" && x["trackId"] == track_id)
            .or(Some(e))
            .and_then(|x| x["t"].as_u64())
            .unwrap_or(0);
        let stopped_t = events
            .iter()
            .find(|x| x["type"] == "track.stopped" && x["trackId"] == track_id)
            .and_then(|x| x["t"].as_u64())
            .or_else(|| file_end_t(file))
            .unwrap_or(last_t);
        tracks.push(TrackManifest {
            track_id: track_id.to_string(),
            peer_id: peer_id.to_string(),
            kind,
            codec: e["codec"].as_str().unwrap_or("").to_string(),
            clock_rate: if kind == TrackKind::Mic { 48000 } else { 90000 },
            file: file.to_string(),
            start_t: started_t,
            end_t: stopped_t.max(started_t),
            bytes,
            first_rtp_ts: 0,
            sender_reports: Vec::new(),
            packets: 0,
            packets_lost: 0,
            gaps: Vec::new(),
        });
    }

    let stopped_at = started_at + last_t;
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("events.jsonl"))?;
    writeln!(
        log,
        "{}",
        serde_json::json!({ "type": "recording.stopped", "at": now_ms(), "t": last_t, "reason": "recorder_lost", "finalisedOnRestart": true })
    )?;

    let mut manifest = Manifest {
        version: 1,
        recording_id,
        prefix: field(started, "prefix"),
        // A spool from before the rename says meetingId; it is the same id.
        scope_id: field(started, "scopeId").or_else(|| field(started, "meetingId")),
        call_id: field(started, "callId"),
        account_id: field(started, "accountId"),
        session_id: field(started, "sessionId"),
        metadata: started["metadata"].as_object().cloned(),
        recorder_id: recorder_id.to_string(),
        started_at,
        title: None,
        started_by_name: None,
        stopped_at,
        stop_reason: StopReason::RecorderLost,
        duration_ms: last_t.max(1),
        policy: Policy {
            name: started["layoutPolicy"]
                .as_str()
                .unwrap_or("auto")
                .to_string(),
            version: started["policyVersion"].as_u64().unwrap_or(1) as u32,
        },
        peers: peers.into_values().collect(),
        tracks,
        composite: None,
        thumbnail: None,
        audio_mix: None,
    };
    manifest.prefix = Some(manifest.storage_prefix());
    let tmp = dir.join("manifest.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join("manifest.json"))?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalises_an_interrupted_spool_from_its_event_log() {
        let dir = std::env::temp_dir().join(format!("recorder-orphan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("tracks/p1")).unwrap();
        std::fs::write(dir.join("tracks/p1/v1.webm"), vec![0u8; 5000]).unwrap();
        std::fs::write(dir.join("tracks/p1/a1.webm"), Vec::<u8>::new()).unwrap(); // never received anything
        let started_at = now_ms() - 30_000;
        let lines = [
            format!(r#"{{"type":"recording.started","at":{started_at},"t":0,"recordingId":"r1","scopeId":"m1","callId":"c1","sessionId":"s1","accountId":"acc","layoutPolicy":"auto","policyVersion":1}}"#),
            r#"{"type":"peer.joined","at":1000,"t":0,"peerId":"p1","userId":"u1","name":"Alice","userType":"Member"}"#.to_string(),
            r#"{"type":"track.allocated","at":1030,"t":30,"trackId":"v1","peerId":"p1","kind":"webcam","codec":"video/VP8","file":"tracks/p1/v1.webm"}"#.to_string(),
            r#"{"type":"track.allocated","at":1030,"t":30,"trackId":"a1","peerId":"p1","kind":"mic","codec":"audio/opus","file":"tracks/p1/a1.webm"}"#.to_string(),
            r#"{"type":"track.started","at":1040,"t":40,"trackId":"v1"}"#.to_string(),
            r#"{"type":"speaker.changed","at":9000,"t":8000,"peerId":"p1"}"#.to_string(),
        ];
        std::fs::write(dir.join("events.jsonl"), lines.join("\n") + "\n").unwrap();

        let m = finalise(&dir, "rec-1").unwrap();
        assert_eq!(m.recording_id, "r1");
        assert_eq!(m.stop_reason, StopReason::RecorderLost);
        assert_eq!(m.storage_prefix(), "acc/m1/r1");
        // The video file was written "now", 30 s after the start: that, not the last event at 8 s, ends it.
        assert!(
            m.duration_ms >= 29_000 && m.duration_ms <= 40_000,
            "duration {}",
            m.duration_ms
        );
        assert_eq!(m.stopped_at, started_at + m.duration_ms);
        assert_eq!(m.peers.len(), 1);
        assert_eq!(m.tracks.len(), 1, "the empty mic file is left out");
        assert_eq!(m.tracks[0].start_t, 40);
        assert!(m.tracks[0].end_t >= 29_000, "end_t {}", m.tracks[0].end_t);
        assert!(dir.join("manifest.json").exists());
        let log = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(log.contains("recording.stopped"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalises_a_spool_without_application_ids() {
        let dir = std::env::temp_dir().join(format!("recorder-orphan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("tracks/p1")).unwrap();
        std::fs::write(dir.join("tracks/p1/v1.webm"), vec![0u8; 10]).unwrap();
        let lines = [
            format!(r#"{{"type":"recording.started","at":{},"t":0,"recordingId":"solo","prefix":"team/solo"}}"#, now_ms()),
            r#"{"type":"track.allocated","at":1,"t":1,"trackId":"v1","peerId":"p1","kind":"webcam","codec":"video/VP8","file":"tracks/p1/v1.webm"}"#.to_string(),
        ];
        std::fs::write(dir.join("events.jsonl"), lines.join("\n") + "\n").unwrap();
        let m = finalise(&dir, "rec-1").unwrap();
        assert_eq!(m.storage_prefix(), "team/solo");
        assert!(m.account_id.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
