//! The control socket controllers connect to (`docs/protocol.md`): socket.io, handshake query
//! `{ clientId, secret }` (`roomServerId` is accepted as an older name), events `notification` and `request` both
//! ways, `recorderReady` once accepted. A `request` is `{ method, data }` answered by an ack of two arguments
//! `(serverError, response)`.

use crate::capture::Capture;
use crate::wire::{
    now_ms, AllocateTrack, CaptureEvent, CaptureStart, CaptureStop, ReleaseTrack, StopReason,
    TrackConnected,
};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::Value;
use socketioxide::extract::{AckSender, SocketRef, State, TryData};
use socketioxide::{SocketIo, TransportType};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct App(pub Arc<Capture>);

#[derive(Debug, Deserialize)]
struct Request {
    method: String,
    #[serde(default)]
    data: Value,
}

pub fn layer(capture: Arc<Capture>) -> (socketioxide::layer::SocketIoLayer, SocketIo) {
    let (layer, io) = SocketIo::builder()
        .with_state(App(capture))
        .transports([TransportType::Websocket, TransportType::Polling])
        .max_payload(4 * 1024 * 1024)
        .build_layer();
    io.ns("/", on_connect);
    (layer, io)
}

async fn on_connect(socket: SocketRef, State(app): State<App>) {
    let query = socket.req_parts().uri.query().unwrap_or("");
    let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let client = params
        .get("clientId")
        .or_else(|| params.get("roomServerId"))
        .cloned()
        .unwrap_or_default();
    let secret = params.get("secret").cloned().unwrap_or_default();
    if client.is_empty() || secret != app.0.config.secret {
        tracing::warn!(socket = %socket.id, "control connection rejected: bad secret or missing clientId");
        socket.disconnect().ok();
        return;
    }
    tracing::info!(socket = %socket.id, client = %client, "controller connected");
    socket.on("request", on_request);
    socket.on("notification", on_notification);
    socket.on_disconnect(on_disconnect);
    let _ = socket.emit(
        "notification",
        &serde_json::json!({ "method": "recorderReady", "data": { "load": app.0.load() } }),
    );
}

async fn on_notification(TryData(req): TryData<Request>) {
    match req {
        Ok(r) => tracing::warn!(method = %r.method, "notification not understood"),
        Err(e) => tracing::warn!(error = %e, "bad notification"),
    }
}

async fn on_request(
    socket: SocketRef,
    TryData(req): TryData<Request>,
    State(app): State<App>,
    ack: AckSender,
) {
    let req = match req {
        Ok(r) => r,
        Err(e) => {
            ack.send(&(Some(format!("bad request: {e}")), Value::Null))
                .ok();
            return;
        }
    };
    let method = req.method.clone();
    let result = handle(&app.0, &socket, &method, req.data).await;
    let load = app.0.load();
    let reply: (Option<String>, Value) = match result {
        Ok(mut v) => {
            if let Value::Object(m) = &mut v {
                m.insert("load".into(), Value::from(load));
            }
            (None, v)
        }
        Err(e) => {
            tracing::warn!(%method, error = %e, "request failed");
            (Some(e.to_string()), Value::Null)
        }
    };
    ack.send(&reply).ok();
}

async fn handle(
    capture: &Arc<Capture>,
    socket: &SocketRef,
    method: &str,
    data: Value,
) -> Result<Value> {
    fn parse<T: for<'de> Deserialize<'de>>(data: Value) -> Result<T> {
        serde_json::from_value(data).map_err(|e| anyhow!("invalid data: {e}"))
    }
    match method {
        "capture.start" => {
            let req: CaptureStart = parse(data)?;
            capture.start(req, socket.clone()).await?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "capture.allocateTrack" => {
            let req: AllocateTrack = parse(data)?;
            let recording = capture.recording(&req.recording_id)?;
            let response = recording.allocate_track(req).await?;
            Ok(serde_json::to_value(response)?)
        }
        "capture.trackConnected" => {
            let req: TrackConnected = parse(data)?;
            let recording = capture.recording(&req.recording_id)?;
            recording.track_connected(
                &req.track_id,
                &req.consumer_id,
                &req.rtcp.ip,
                req.rtcp.port,
            )?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "capture.releaseTrack" => {
            let req: ReleaseTrack = parse(data)?;
            let recording = capture.recording(&req.recording_id)?;
            recording.release_track(&req.track_id).await?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "capture.event" => {
            let req: CaptureEvent = parse(data)?;
            let recording = capture.recording(&req.recording_id)?;
            let mut event = req.event;
            if !event.contains_key("type") {
                return Err(anyhow!("event without type"));
            }
            event.entry("at").or_insert_with(|| Value::from(now_ms()));
            recording.event(Value::Object(event))?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "capture.stop" => {
            let req: CaptureStop = parse(data)?;
            let recording = capture.recording(&req.recording_id)?;
            recording.stop(req.reason).await?;
            Ok(serde_json::json!({ "ok": true }))
        }
        // Unknown input fails closed.
        other => Err(anyhow!("unknown method {other}")),
    }
}

/// Controller gone: its captures get the orphan grace, then finalise on their own.
async fn on_disconnect(socket: SocketRef, State(app): State<App>) {
    let orphans = app.0.recordings_owned_by(&socket);
    if orphans.is_empty() {
        return;
    }
    let grace = Duration::from_millis(app.0.config.orphan_grace_ms);
    tracing::warn!(socket = %socket.id, count = orphans.len(), grace_ms = grace.as_millis() as u64, "controller lost; captures orphaned");
    for recording in orphans {
        *recording.owner.lock().unwrap() = None;
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            // No reconnect claimed it (there is no re-claim in version one): finalise what we have.
            if !recording.is_stopped() {
                if let Err(e) = recording.stop(StopReason::RoomServerLost).await {
                    tracing::error!(recording = %recording.id, error = %e, "orphan stop failed");
                }
            }
        });
    }
}
