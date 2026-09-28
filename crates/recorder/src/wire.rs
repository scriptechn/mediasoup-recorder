//! The wire vocabulary (`docs/protocol.md`, `docs/spool-format.md`): closed lists shared with the controller and
//! whatever consumes the status channel. Unknown values fail closed: serde rejects them at the boundary.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    Starting,
    Recording,
    Stopping,
    Captured,
    Composing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    User,
    RoomClosed,
    MediaNodeLost,
    RecorderLost,
    RoomServerLost,
    DiskFull,
    MaxDuration,
    Retention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    CaptureNoMedia,
    UploadFailed,
    ComposeFailed,
    SpoolMissing,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Mic,
    Webcam,
    Screen,
}

/// A peer as the controller describes it in `capture.start` and `peer.joined`. Only `peerId` matters to the
/// recorder; the rest is for the composite's labels and slates and for whoever reads the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub peer_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
    /// Free-form: whatever the application uses to classify participants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub name: String,
    pub version: u32,
}

// ---- requests controller → recorder

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStart {
    pub recording_id: String,
    /// Where the recording goes in the bucket, relative to the bucket root, without a trailing slash. When absent
    /// it is `<accountId>/<scopeId>/<recordingId>` if both ids are given, else `<recordingId>`.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Application identifiers, all optional: copied into the manifest and the event log, and used for the
    /// default prefix. `meetingId` is an older name for `scopeId`.
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default, alias = "meetingId")]
    pub scope_id: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Anything else the application wants to find in the manifest later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub started_at: u64,
    /// For the title card: the meeting or room title and who pressed record; both optional.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub started_by_name: Option<String>,
    #[serde(default = "default_policy")]
    pub policy: Policy,
    #[serde(default)]
    pub peers: Vec<PeerInfo>,
}

fn default_policy() -> Policy {
    Policy {
        name: "auto".into(),
        version: 1,
    }
}

impl CaptureStart {
    /// The bucket prefix (and the compose queue item) for this recording.
    pub fn storage_prefix(&self) -> String {
        if let Some(p) = self.prefix.as_deref().map(|p| p.trim_matches('/')) {
            if !p.is_empty() {
                return p.to_string();
            }
        }
        match (&self.account_id, &self.scope_id) {
            (Some(a), Some(s)) if !a.is_empty() && !s.is_empty() => {
                format!("{a}/{s}/{}", self.recording_id)
            }
            _ => self.recording_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocateTrack {
    pub recording_id: String,
    pub track_id: String,
    pub peer_id: String,
    pub kind: TrackKind,
    pub codec: String,
    pub clock_rate: u32,
    pub rtp_parameters: RtpParameters,
}

/// The RTP parameters of the stream the SFU will send: the subset of mediasoup's consumer `rtpParameters` the
/// pipeline needs (payload types, SSRCs, the orientation header extension).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtpParameters {
    pub codecs: Vec<RtpCodec>,
    #[serde(default)]
    pub encodings: Vec<RtpEncoding>,
    #[serde(default)]
    pub header_extensions: Vec<RtpHeaderExtensionParameters>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtpHeaderExtensionParameters {
    pub uri: String,
    pub id: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtpCodec {
    pub mime_type: String,
    pub payload_type: u8,
    pub clock_rate: u32,
    #[serde(default)]
    pub channels: Option<u8>,
    #[serde(default)]
    pub parameters: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtpEncoding {
    #[serde(default)]
    pub ssrc: Option<u32>,
    #[serde(default)]
    pub rtx: Option<RtxEncoding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtxEncoding {
    pub ssrc: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocateTrackResponse {
    pub ip: String,
    pub port: u16,
    pub rtcp_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackConnected {
    pub recording_id: String,
    pub track_id: String,
    /// The SFU's own id for the stream, echoed in `needKeyFrame` so the controller can find it.
    #[serde(default)]
    pub consumer_id: String,
    pub rtcp: RtcpTarget,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RtcpTarget {
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseTrack {
    pub recording_id: String,
    pub track_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureEvent {
    pub recording_id: String,
    /// One `events.jsonl` line without `t`; `type` is required, `at` is stamped when missing, the rest is passed
    /// through.
    pub event: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStop {
    pub recording_id: String,
    pub reason: StopReason,
}

// ---- status recorder → application (Redis channel `recording-events`)

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEvent {
    pub recording_id: String,
    pub status: RecordingStatus,
    pub at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_reason: Option<FailReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_detail: Option<String>,
    /// Keys in the bucket, relative to the recording prefix (`captured` and `ready`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<String>>,
}

/// The registry entry in the `recorders:` hash: what a controller needs to pick and reach a recorder.
#[derive(Debug, Clone, Serialize)]
pub struct RegistryEntry<'a> {
    pub id: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub tls: bool,
    pub load: f64,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(json: serde_json::Value) -> CaptureStart {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn prefix_is_explicit_then_derived_then_the_id() {
        let explicit = start(serde_json::json!({
            "recordingId": "r1", "startedAt": 1, "prefix": "/tenant-a/room-7/",
            "accountId": "acc", "scopeId": "grp"
        }));
        assert_eq!(explicit.storage_prefix(), "tenant-a/room-7");
        let derived = start(serde_json::json!({
            "recordingId": "r1", "startedAt": 1, "accountId": "acc", "meetingId": "grp"
        }));
        assert_eq!(derived.storage_prefix(), "acc/grp/r1");
        let bare = start(serde_json::json!({ "recordingId": "r1", "startedAt": 1 }));
        assert_eq!(bare.storage_prefix(), "r1");
        assert_eq!(bare.policy.name, "auto");
        assert!(bare.peers.is_empty());
    }
}
