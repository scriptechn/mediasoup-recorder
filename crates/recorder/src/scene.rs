//! Scene evaluation (`docs/layout.md`): a pure function of the event log, the manifest and the
//! policy. The result is a timeline of layouts, one per change point, that `compose` turns into compositor
//! geometry. Nothing here touches GStreamer, so it is unit-tested on its own.

use crate::policy::Policy;
use crate::spool::{Manifest, TrackManifest};
use crate::wire::TrackKind;
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};

/// One `events.jsonl` line: `type`, `t` (ms since `recording.started`) and the rest.
#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub t: u64,
    #[serde(flatten)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl Event {
    fn str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(|v| v.as_str())
    }
}

pub fn parse_events(text: &str) -> Vec<Event> {
    let mut events: Vec<Event> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    events.sort_by_key(|e| e.t);
    events
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// Frames of this track file exist at `t`.
    Video { track_id: String },
    /// Avatar slate: initials on the peer's signature colour.
    Slate,
}

/// What a tile shows; `Stage` is the shared screen, `Tile` a participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Stage,
    Tile,
}

/// What the name label shows next to the name, as icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TileStatus {
    pub muted: bool,
    pub hand: bool,
    pub sharing: bool,
    pub hold: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub peer_id: String,
    pub role: Role,
    pub rect: Rect,
    pub content: Content,
    pub speaker: bool,
    /// Name plus status words; one label image per distinct string (and the tests' view of the tile).
    pub label: String,
    pub name: String,
    pub status: TileStatus,
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub t: u64,
    pub tiles: Vec<Tile>,
    /// Peers with no tile this scene (grid overflow), by name, for the bottom strip.
    pub strip: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Timeline {
    pub scenes: Vec<Scene>,
    pub duration_ms: u64,
    pub names: HashMap<String, String>,
}

impl Timeline {
    /// Every (peer, role) pair that ever gets a tile: one slate, label and border pad each in the composite.
    pub fn slots(&self) -> BTreeSet<(String, Role)> {
        self.scenes
            .iter()
            .flat_map(|s| s.tiles.iter().map(|t| (t.peer_id.clone(), t.role)))
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
struct PeerState {
    name: String,
    joined: bool,
    join_order: usize,
    camera: Option<String>,
    camera_paused: bool,
    mic: Option<String>,
    mic_paused: bool,
    share: Option<String>,
    hand: bool,
    hold: bool,
}

#[derive(Debug, Default)]
struct State {
    peers: Vec<(String, PeerState)>,
    joins: usize,
    /// Most recent first, reordered only after the speaker held the floor `speakerHoldMs`.
    speakers: Vec<String>,
    current_speaker: Option<String>,
    pending_speaker: Option<(String, u64)>,
    /// Active shares in start order; the most recent wins the stage.
    shares: Vec<(String, String)>,
    track_kind: HashMap<String, (String, TrackKind)>,
}

impl State {
    fn peer_mut(&mut self, id: &str) -> &mut PeerState {
        if let Some(i) = self.peers.iter().position(|(p, _)| p == id) {
            return &mut self.peers[i].1;
        }
        self.peers.push((id.to_string(), PeerState::default()));
        &mut self.peers.last_mut().unwrap().1
    }

    fn apply(&mut self, e: &Event, policy: &Policy) {
        match e.kind.as_str() {
            "peer.joined" => {
                let Some(id) = e.str("peerId") else { return };
                let name = e.str("name").unwrap_or("").to_string();
                let order = self.joins;
                self.joins += 1;
                let p = self.peer_mut(id);
                if !name.is_empty() {
                    p.name = name;
                }
                if !p.joined {
                    p.joined = true;
                    p.join_order = order;
                }
            }
            "peer.left" => {
                let Some(id) = e.str("peerId") else { return };
                let p = self.peer_mut(id);
                p.joined = false;
                p.camera = None;
                p.mic = None;
                p.share = None;
                p.hand = false;
                p.hold = false;
                self.shares.retain(|(peer, _)| peer != id);
                self.speakers.retain(|s| s != id);
            }
            "peer.renamed" => {
                if let (Some(id), Some(name)) = (e.str("peerId"), e.str("name")) {
                    self.peer_mut(id).name = name.to_string();
                }
            }
            "track.allocated" => {
                if let (Some(track), Some(peer), Some(kind)) =
                    (e.str("trackId"), e.str("peerId"), e.str("kind"))
                {
                    let kind = match kind {
                        "mic" => TrackKind::Mic,
                        "screen" => TrackKind::Screen,
                        _ => TrackKind::Webcam,
                    };
                    self.track_kind
                        .insert(track.to_string(), (peer.to_string(), kind));
                }
            }
            "track.started" | "track.resumed" | "track.paused" | "track.stopped" => {
                let Some(track) = e.str("trackId") else {
                    return;
                };
                let Some((peer, kind)) = self.track_kind.get(track).cloned() else {
                    return;
                };
                let started = e.kind == "track.started";
                let stopped = e.kind == "track.stopped";
                let paused = e.kind == "track.paused";
                let p = self.peer_mut(&peer);
                match kind {
                    TrackKind::Webcam => {
                        if started {
                            p.camera = Some(track.to_string());
                            p.camera_paused = false;
                        } else if stopped && p.camera.as_deref() == Some(track) {
                            p.camera = None;
                        } else if p.camera.as_deref() == Some(track) {
                            p.camera_paused = paused;
                        }
                    }
                    TrackKind::Mic => {
                        if started {
                            p.mic = Some(track.to_string());
                            p.mic_paused = false;
                        } else if stopped && p.mic.as_deref() == Some(track) {
                            p.mic = None;
                        } else if p.mic.as_deref() == Some(track) {
                            p.mic_paused = paused;
                        }
                    }
                    TrackKind::Screen => {
                        if started {
                            p.share = Some(track.to_string());
                            self.shares.retain(|(_, t)| t != track);
                            self.shares.push((peer.clone(), track.to_string()));
                        } else if stopped {
                            if p.share.as_deref() == Some(track) {
                                p.share = None;
                            }
                            self.shares.retain(|(_, t)| t != track);
                        }
                    }
                }
            }
            "share.started" => {
                if let (Some(peer), Some(track)) = (e.str("peerId"), e.str("trackId")) {
                    self.track_kind
                        .insert(track.to_string(), (peer.to_string(), TrackKind::Screen));
                    self.peer_mut(peer).share = Some(track.to_string());
                    self.shares.retain(|(_, t)| t != track);
                    self.shares.push((peer.to_string(), track.to_string()));
                }
            }
            "share.stopped" => {
                if let Some(track) = e.str("trackId") {
                    self.shares.retain(|(_, t)| t != track);
                    for (_, p) in self.peers.iter_mut() {
                        if p.share.as_deref() == Some(track) {
                            p.share = None;
                        }
                    }
                }
            }
            "speaker.changed" => {
                let Some(id) = e.str("peerId") else { return };
                self.current_speaker = Some(id.to_string());
                if self.speakers.first().map(|s| s.as_str()) == Some(id) {
                    self.pending_speaker = None;
                } else if self.speakers.is_empty() {
                    self.speakers.insert(0, id.to_string());
                    self.pending_speaker = None;
                } else {
                    self.pending_speaker = Some((id.to_string(), e.t + policy.speaker_hold_ms));
                }
            }
            "hand.raised" | "hand.lowered" => {
                if let Some(id) = e.str("peerId") {
                    self.peer_mut(id).hand = e.kind == "hand.raised";
                }
            }
            "peer.hold" | "peer.unhold" => {
                if let Some(id) = e.str("peerId") {
                    self.peer_mut(id).hold = e.kind == "peer.hold";
                }
            }
            _ => {}
        }
    }

    /// A speaker who kept the floor for the hold time moves to the front of the recent list.
    fn settle_speaker(&mut self, t: u64) {
        if let Some((id, due)) = self.pending_speaker.clone() {
            if t >= due && self.current_speaker.as_deref() == Some(&id) {
                self.speakers.retain(|s| s != &id);
                self.speakers.insert(0, id);
                self.pending_speaker = None;
            }
        }
    }

    fn joined(&self) -> Vec<&(String, PeerState)> {
        let mut v: Vec<_> = self.peers.iter().filter(|(_, p)| p.joined).collect();
        v.sort_by_key(|(_, p)| p.join_order);
        v
    }

    /// Recent speakers first, then everyone else in join order.
    fn by_recency(&self) -> Vec<String> {
        let joined = self.joined();
        let mut out: Vec<String> = self
            .speakers
            .iter()
            .filter(|s| joined.iter().any(|(id, _)| id == *s))
            .cloned()
            .collect();
        for (id, _) in joined {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }
}

/// The web gallery's fit (`gallery-view.tsx`): the row count where equal 16:9 tiles fill the box best.
pub fn grid_fit(
    boxes: usize,
    width: u32,
    height: u32,
    gap: u32,
    aspect: f64,
) -> (usize, usize, u32, u32) {
    if boxes == 0 {
        return (0, 0, 0, 0);
    }
    let (width, height, gap) = (width as f64, height as f64, gap as f64);
    let mut best = (1usize, boxes, 0.0f64, 0.0f64);
    for rows in 1..=boxes {
        let columns = boxes.div_ceil(rows);
        let vertical_gaps = rows as f64 * gap;
        let horizontal_gaps = columns as f64 * gap;
        let mut x = (width - horizontal_gaps) / columns as f64;
        let mut y = x / aspect;
        if height - vertical_gaps < y * rows as f64 {
            y = (height - vertical_gaps) / rows as f64;
            x = aspect * y;
            best = (rows, columns, x, y);
            break;
        }
        best = (rows, columns, x, y);
        let space = (height - vertical_gaps) - y * rows as f64;
        if space < y {
            break;
        }
    }
    (best.0, best.1, best.2.floor() as u32, best.3.floor() as u32)
}

/// Equal tiles centred in the box, rows centred too (the last row may be short).
fn grid_rects(boxes: usize, area: Rect, gap: u32, aspect: f64) -> Vec<Rect> {
    let (rows, cols, w, h) = grid_fit(boxes, area.w, area.h, gap, aspect);
    if boxes == 0 || w == 0 || h == 0 {
        return Vec::new();
    }
    let total_h = rows as u32 * h + (rows as u32 - 1) * gap;
    let y0 = area.y + ((area.h as i32 - total_h as i32) / 2).max(0);
    let mut rects = Vec::with_capacity(boxes);
    for row in 0..rows {
        let in_row = if row + 1 == rows {
            boxes - row * cols
        } else {
            cols
        };
        let total_w = in_row as u32 * w + (in_row as u32 - 1) * gap;
        let x0 = area.x + ((area.w as i32 - total_w as i32) / 2).max(0);
        for col in 0..in_row {
            rects.push(Rect {
                x: x0 + col as i32 * (w + gap) as i32,
                y: y0 + row as i32 * (h + gap) as i32,
                w,
                h,
            });
        }
    }
    rects
}

fn display_name(p: &PeerState) -> String {
    if p.name.is_empty() {
        "Guest".to_string()
    } else {
        p.name.clone()
    }
}

fn status_for(p: &PeerState, sharing: bool) -> TileStatus {
    TileStatus {
        sharing,
        hold: p.hold,
        muted: !p.hold && (p.mic.is_none() || p.mic_paused),
        hand: p.hand,
    }
}

fn label_for(p: &PeerState, sharing: bool) -> String {
    let mut label = display_name(p);
    if sharing {
        label.push_str(" (sharing screen)");
    }
    if p.hold {
        label.push_str(" · on hold");
    } else if p.mic.is_none() || p.mic_paused {
        label.push_str(" · muted");
    }
    if p.hand {
        label.push_str(" · hand raised");
    }
    label
}

fn file_has_frames(track: Option<&TrackManifest>, t: u64) -> bool {
    match track {
        Some(tr) => t >= tr.start_t && t < tr.end_t.max(tr.start_t + 1),
        None => false,
    }
}

fn camera_content(p: &PeerState, tracks: &HashMap<String, &TrackManifest>, t: u64) -> Content {
    match &p.camera {
        Some(track) if !p.camera_paused && !p.hold => {
            if file_has_frames(tracks.get(track).copied(), t) {
                Content::Video {
                    track_id: track.clone(),
                }
            } else {
                Content::Slate
            }
        }
        _ => Content::Slate,
    }
}

fn layout(
    state: &State,
    tracks: &HashMap<String, &TrackManifest>,
    t: u64,
    policy: &Policy,
) -> Scene {
    let canvas = policy.canvas;
    let gap = policy.gap_px;
    let joined = state.joined();
    let speaker = state.current_speaker.clone();
    let mut tiles = Vec::new();
    let mut strip = Vec::new();

    let share = state.shares.last().filter(|(peer, track)| {
        joined.iter().any(|(id, _)| id == peer) && file_has_frames(tracks.get(track).copied(), t)
    });

    if let Some((sharer, track)) = share {
        let stage_w = (canvas.width as f64 * policy.share_stage_width_ratio) as u32 - gap;
        let stage = Rect {
            x: gap as i32,
            y: gap as i32,
            w: stage_w,
            h: canvas.height - 2 * gap,
        };
        let sharer_state = &joined.iter().find(|(id, _)| id == sharer).unwrap().1;
        tiles.push(Tile {
            peer_id: sharer.clone(),
            role: Role::Stage,
            rect: stage,
            content: Content::Video {
                track_id: track.clone(),
            },
            speaker: false,
            label: label_for(sharer_state, true),
            name: display_name(sharer_state),
            status: status_for(sharer_state, true),
        });
        // Filmstrip: the sharer first, then the most recent speakers.
        let mut order = vec![sharer.clone()];
        for id in state.by_recency() {
            if !order.contains(&id) {
                order.push(id);
            }
        }
        order.truncate(policy.filmstrip_tiles);
        let strip_x = stage.x + stage.w as i32 + gap as i32;
        let strip_w = (canvas.width as i32 - strip_x - gap as i32).max(1) as u32;
        let mut tile_h = (strip_w as f64 / policy.aspect_ratio) as u32;
        let n = order.len() as u32;
        if n > 0 && n * tile_h + (n - 1) * gap > canvas.height - 2 * gap {
            tile_h = (canvas.height - 2 * gap - (n - 1) * gap) / n;
        }
        let tile_w = (tile_h as f64 * policy.aspect_ratio) as u32;
        let total_h = n * tile_h + n.saturating_sub(1) * gap;
        let y0 = ((canvas.height as i32 - total_h as i32) / 2).max(gap as i32);
        for (i, id) in order.iter().enumerate() {
            let p = &joined.iter().find(|(pid, _)| pid == id).unwrap().1;
            tiles.push(Tile {
                peer_id: id.clone(),
                role: Role::Tile,
                rect: Rect {
                    x: strip_x + ((strip_w as i32 - tile_w as i32) / 2).max(0),
                    y: y0 + i as i32 * (tile_h + gap) as i32,
                    w: tile_w,
                    h: tile_h,
                },
                content: camera_content(p, tracks, t),
                speaker: speaker.as_deref() == Some(id),
                label: label_for(p, false),
                name: display_name(p),
                status: status_for(p, false),
            });
        }
        for (id, p) in &joined {
            if !order.contains(id) {
                strip.push(if p.name.is_empty() {
                    "Guest".into()
                } else {
                    p.name.clone()
                });
            }
        }
    } else {
        let mut order: Vec<String> = joined.iter().map(|(id, _)| id.clone()).collect();
        if order.len() > policy.max_grid_tiles {
            let recent = state.by_recency();
            let shown: Vec<String> = recent.into_iter().take(policy.max_grid_tiles).collect();
            for (id, p) in &joined {
                if !shown.contains(id) {
                    strip.push(if p.name.is_empty() {
                        "Guest".into()
                    } else {
                        p.name.clone()
                    });
                }
            }
            order.retain(|id| shown.contains(id));
        }
        let strip_h = if strip.is_empty() {
            0
        } else {
            policy.name_strip_height_px
        };
        let area = Rect {
            x: gap as i32,
            y: gap as i32,
            w: canvas.width - 2 * gap,
            h: canvas.height - 2 * gap - strip_h,
        };
        let rects = grid_rects(order.len(), area, gap, policy.aspect_ratio);
        for (id, rect) in order.iter().zip(rects) {
            let p = &joined.iter().find(|(pid, _)| pid == id).unwrap().1;
            tiles.push(Tile {
                peer_id: id.clone(),
                role: Role::Tile,
                rect,
                content: camera_content(p, tracks, t),
                speaker: speaker.as_deref() == Some(id),
                label: label_for(p, false),
                name: display_name(p),
                status: status_for(p, false),
            });
        }
    }
    Scene { t, tiles, strip }
}

/// The timeline: one scene per change point (every event, every track file edge, every speaker settle time).
pub fn evaluate(manifest: &Manifest, events: &[Event], policy: &Policy) -> Timeline {
    let tracks: HashMap<String, &TrackManifest> = manifest
        .tracks
        .iter()
        .map(|t| (t.track_id.clone(), t))
        .collect();
    let mut points: BTreeSet<u64> = events.iter().map(|e| e.t).collect();
    for t in &manifest.tracks {
        points.insert(t.start_t);
        points.insert(t.end_t);
    }
    points.insert(0);
    // Speaker settle times need the events applied; collect them in a first pass.
    for e in events {
        if e.kind == "speaker.changed" {
            points.insert(e.t + policy.speaker_hold_ms);
        }
    }

    let mut state = State::default();
    for t in &manifest.tracks {
        state
            .track_kind
            .insert(t.track_id.clone(), (t.peer_id.clone(), t.kind));
    }
    let mut names = HashMap::new();
    for p in &manifest.peers {
        if let Some(n) = &p.name {
            names.insert(p.peer_id.clone(), n.clone());
        }
    }
    // An orientation change alters no tile but the compose must re-fit the picture there: force a scene.
    let forced: BTreeSet<u64> = events
        .iter()
        .filter(|e| e.kind == "recorder.orientation")
        .map(|e| e.t)
        .collect();
    let mut scenes = Vec::new();
    let mut next = 0usize;
    let mut last: Option<Vec<Tile>> = None;
    let mut last_strip: Vec<String> = Vec::new();
    for &t in points.iter().filter(|&&t| t <= manifest.duration_ms) {
        while next < events.len() && events[next].t <= t {
            state.apply(&events[next], policy);
            next += 1;
        }
        state.settle_speaker(t);
        let scene = layout(&state, &tracks, t, policy);
        let same = last.as_ref() == Some(&scene.tiles) && last_strip == scene.strip;
        if !same || scenes.is_empty() || forced.contains(&t) {
            last = Some(scene.tiles.clone());
            last_strip = scene.strip.clone();
            scenes.push(scene);
        }
    }
    for (id, p) in &state.peers {
        if !p.name.is_empty() {
            names.insert(id.clone(), p.name.clone());
        }
    }
    Timeline {
        scenes,
        duration_ms: manifest.duration_ms,
        names,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spool::Manifest;
    use crate::wire::{PeerInfo, Policy as ManifestPolicy, StopReason};

    fn policy() -> Policy {
        serde_json::from_str(include_str!("../../../policies/auto.json")).unwrap()
    }

    fn track(id: &str, peer: &str, kind: TrackKind, start: u64, end: u64) -> TrackManifest {
        TrackManifest {
            track_id: id.into(),
            peer_id: peer.into(),
            kind,
            codec: "video/VP8".into(),
            clock_rate: 90000,
            file: format!("tracks/{peer}/{id}.webm"),
            start_t: start,
            end_t: end,
            bytes: 1,
            first_rtp_ts: 0,
            sender_reports: vec![],
            packets: 1,
            packets_lost: 0,
            gaps: vec![],
        }
    }

    fn manifest(tracks: Vec<TrackManifest>, duration: u64) -> Manifest {
        Manifest {
            version: 1,
            recording_id: "r".into(),
            prefix: None,
            scope_id: Some("m".into()),
            call_id: Some("c".into()),
            account_id: Some("a".into()),
            session_id: Some("s".into()),
            metadata: None,
            recorder_id: "rec".into(),
            started_at: 0,
            title: None,
            started_by_name: None,
            stopped_at: duration,
            stop_reason: StopReason::User,
            duration_ms: duration,
            policy: ManifestPolicy {
                name: "auto".into(),
                version: 1,
            },
            peers: vec![PeerInfo {
                peer_id: "A".into(),
                user_id: "ua".into(),
                name: Some("Alice".into()),
                picture: None,
                user_type: None,
            }],
            tracks,
            composite: None,
            thumbnail: None,
            audio_mix: None,
        }
    }

    fn ev(t: u64, kind: &str, fields: serde_json::Value) -> Event {
        let mut fields = fields.as_object().cloned().unwrap_or_default();
        fields.insert("t".into(), t.into());
        fields.insert("type".into(), kind.into());
        serde_json::from_value(serde_json::Value::Object(fields)).unwrap()
    }

    #[test]
    fn grid_fit_matches_web_for_small_counts() {
        let (rows, cols, w, h) = grid_fit(1, 1264, 704, 8, 16.0 / 9.0);
        assert_eq!((rows, cols), (1, 1));
        assert!(w <= 1256 && h <= 696 && w > 1000);
        let (rows, cols, _, _) = grid_fit(2, 1264, 704, 8, 16.0 / 9.0);
        assert_eq!((rows, cols), (1, 2));
        let (rows, cols, _, _) = grid_fit(4, 1264, 704, 8, 16.0 / 9.0);
        assert_eq!((rows, cols), (2, 2));
        let (rows, cols, _, _) = grid_fit(9, 1264, 704, 8, 16.0 / 9.0);
        assert_eq!((rows, cols), (3, 3));
    }

    #[test]
    fn tiles_never_overlap_and_stay_on_canvas() {
        let p = policy();
        for n in 1..=9 {
            let area = Rect {
                x: 8,
                y: 8,
                w: 1264,
                h: 704,
            };
            let rects = grid_rects(n, area, 8, p.aspect_ratio);
            assert_eq!(rects.len(), n);
            for (i, a) in rects.iter().enumerate() {
                assert!(a.x >= 0 && a.y >= 0);
                assert!(a.x as u32 + a.w <= 1280 && a.y as u32 + a.h <= 720);
                for b in rects.iter().skip(i + 1) {
                    let overlap = a.x < b.x + b.w as i32
                        && b.x < a.x + a.w as i32
                        && a.y < b.y + b.h as i32
                        && b.y < a.y + a.h as i32;
                    assert!(!overlap, "{n} tiles: {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn camera_shows_video_only_while_the_file_has_frames() {
        let p = policy();
        let m = manifest(vec![track("v1", "A", TrackKind::Webcam, 500, 9000)], 10000);
        let events = vec![
            ev(
                0,
                "peer.joined",
                serde_json::json!({"peerId":"A","name":"Alice"}),
            ),
            ev(
                30,
                "track.allocated",
                serde_json::json!({"trackId":"v1","peerId":"A","kind":"webcam"}),
            ),
            ev(40, "track.started", serde_json::json!({"trackId":"v1"})),
            ev(10000, "recording.stopped", serde_json::json!({})),
        ];
        let tl = evaluate(&m, &events, &p);
        let at = |t: u64| {
            tl.scenes.iter().rev().find(|s| s.t <= t).unwrap().tiles[0]
                .content
                .clone()
        };
        assert_eq!(at(100), Content::Slate);
        assert_eq!(
            at(600),
            Content::Video {
                track_id: "v1".into()
            }
        );
        assert_eq!(at(9500), Content::Slate);
        assert_eq!(tl.scenes[0].tiles[0].label, "Alice · muted");
    }

    #[test]
    fn share_takes_the_stage_and_the_sharer_leads_the_filmstrip() {
        let p = policy();
        let m = manifest(
            vec![
                track("v1", "A", TrackKind::Webcam, 0, 10000),
                track("s1", "B", TrackKind::Screen, 2000, 8000),
            ],
            10000,
        );
        let events = vec![
            ev(
                0,
                "peer.joined",
                serde_json::json!({"peerId":"A","name":"Alice"}),
            ),
            ev(
                0,
                "peer.joined",
                serde_json::json!({"peerId":"B","name":"Bob"}),
            ),
            ev(
                0,
                "track.allocated",
                serde_json::json!({"trackId":"v1","peerId":"A","kind":"webcam"}),
            ),
            ev(0, "track.started", serde_json::json!({"trackId":"v1"})),
            ev(
                2000,
                "track.allocated",
                serde_json::json!({"trackId":"s1","peerId":"B","kind":"screen"}),
            ),
            ev(
                2000,
                "share.started",
                serde_json::json!({"peerId":"B","trackId":"s1"}),
            ),
            ev(2000, "track.started", serde_json::json!({"trackId":"s1"})),
            ev(
                8000,
                "share.stopped",
                serde_json::json!({"peerId":"B","trackId":"s1"}),
            ),
            ev(8000, "track.stopped", serde_json::json!({"trackId":"s1"})),
        ];
        let tl = evaluate(&m, &events, &p);
        let scene = tl.scenes.iter().rev().find(|s| s.t <= 5000).unwrap();
        assert_eq!(scene.tiles[0].role, Role::Stage);
        assert_eq!(scene.tiles[0].peer_id, "B");
        assert!(scene.tiles[0].label.contains("sharing screen"));
        assert_eq!(scene.tiles[1].peer_id, "B");
        assert_eq!(scene.tiles[2].peer_id, "A");
        assert!(scene.tiles[0].rect.w > 900);
        let after = tl.scenes.iter().rev().find(|s| s.t <= 9000).unwrap();
        assert!(after.tiles.iter().all(|t| t.role == Role::Tile));
        assert_eq!(after.tiles.len(), 2);
    }

    #[test]
    fn twelve_peers_overflow_into_the_strip() {
        let p = policy();
        let m = manifest(vec![], 1000);
        let mut events = Vec::new();
        for i in 0..12 {
            events.push(ev(
                0,
                "peer.joined",
                serde_json::json!({"peerId": format!("P{i}"), "name": format!("Peer {i}")}),
            ));
        }
        let tl = evaluate(&m, &events, &p);
        let scene = &tl.scenes[0];
        assert_eq!(scene.tiles.len(), 9);
        assert_eq!(scene.strip.len(), 3);
    }
}
