//! The compose role (`docs/architecture.md`, `docs/layout.md`): one offline GStreamer pipeline per recording.
//!
//! ```text
//! tracks/*.webm ──▶ demux ▶ decode ─┐ (pad offset = the track's place on the recording clock, from the
//! slates, labels, borders (appsrc) ─┤  sender-report mapping and firstRtpTs, `docs/spool-format.md`)
//!                                   ├─▶ compositor ▶ x264enc ▶ mp4mux ▶ composite.mp4   (+ thumbnail.jpg)
//! mic tracks ──▶ demux ▶ opusdec ───┴─▶ audiomixer ▶ aac ▶ tee ▶ (mp4mux above, and audio.m4a)
//! ```
//!
//! Tile geometry over time comes from `scene::evaluate`, applied to the compositor pads with control sources
//! keyed on the recording clock (`xpos`, `ypos`, `width`, `height` interpolate over `transitionMs`; `alpha`
//! steps), so the pipeline runs as fast as the CPU allows and stays deterministic.

use crate::policy::Policy;
use crate::raster::{Image, Typeface};
use crate::scene::{self, Content, Rect, Role, Timeline};
use crate::spool::{AudioMixInfo, CompositeInfo, Manifest, ThumbnailInfo, TrackManifest};
use crate::wire::{now_ms, TrackKind};
use anyhow::{anyhow, bail, Context, Result};
use gst::prelude::*;
use gst_controller::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_controller as gst_controller;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub struct ComposeOptions {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    pub font: PathBuf,
    pub icon_font: PathBuf,
    pub brand: String,
    /// Answers "should the render wait right now?" (a live capture on the same box); polled every few seconds,
    /// the pipeline pauses while it says yes.
    pub hold: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

pub struct ComposeResult {
    pub manifest: Manifest,
    pub composite: PathBuf,
    pub thumbnail: PathBuf,
    pub audio_mix: Option<PathBuf>,
}

/// Compose `<dir>/manifest.json` + `events.jsonl` + tracks into `<dir>/composite.mp4`, `thumbnail.jpg` and
/// `audio.m4a`, and return the manifest with the compose entries filled in (not yet written).
pub fn compose(dir: &Path, policy: &Policy, opts: &ComposeOptions) -> Result<ComposeResult> {
    let started = Instant::now();
    let manifest_text =
        std::fs::read_to_string(dir.join("manifest.json")).context("manifest.json")?;
    let mut manifest: Manifest =
        serde_json::from_str(&manifest_text).context("manifest.json is not valid")?;
    let events =
        scene::parse_events(&std::fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default());
    let font = Typeface::load(&opts.font, Some(&opts.icon_font))?;

    // Tracks whose file is not there are composed as camera off and listed.
    let mut missing = Vec::new();
    let present: Vec<TrackManifest> = manifest
        .tracks
        .iter()
        .filter(|t| {
            let ok = dir.join(&t.file).is_file() && t.bytes > 0;
            if !ok {
                missing.push(t.file.clone());
            }
            ok
        })
        .cloned()
        .collect();
    // The composite ends a second after the last media, not when the recorder gave up (a controller lost
    // mid-meeting keeps the capture open for its grace period with nothing arriving).
    let last_media = present.iter().map(|t| t.end_t).max();
    let duration_ms = match last_media {
        Some(end) => manifest.duration_ms.min(end + 200).max(1),
        None => manifest.duration_ms.max(1),
    };
    let scene_manifest = Manifest {
        tracks: present.clone(),
        duration_ms,
        ..manifest.clone()
    };
    let timeline = scene::evaluate(&scene_manifest, &events, policy);
    let offsets = alignment(&present);
    tracing::info!(recording = %manifest.recording_id, scenes = timeline.scenes.len(), tracks = present.len(),
        missing = missing.len(), duration_ms, "compose starting");

    let composite = dir.join("composite.mp4");
    let thumbnail = dir.join("thumbnail.jpg");
    let audio_mix = dir.join("audio.m4a");
    let has_audio = present.iter().any(|t| t.kind == TrackKind::Mic);

    let pipeline = gst::Pipeline::with_name("compose");
    let comp = make("compositor", "comp")?;
    comp.set_property_from_str("background", "black");
    // Every field fixed: a camera joining later must not renegotiate the encoder (a new SPS makes mp4mux rewrite
    // a bigger header over the first fragment at EOS, and the file is unreadable).
    let out_caps = gst::Caps::builder("video/x-raw")
        .field("format", "I420")
        .field("width", opts.width as i32)
        .field("height", opts.height as i32)
        .field("framerate", gst::Fraction::new(opts.fps as i32, 1))
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .field("interlace-mode", "progressive")
        .field("colorimetry", "bt709")
        .field("chroma-site", "mpeg2")
        .build();
    // The compositor blends in BGRA so the labels' and slates' alpha counts (blending in I420 would drop it);
    // videoconvert then hands the encoder the pinned I420.
    let blend_caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", opts.width as i32)
        .field("height", opts.height as i32)
        .field("framerate", gst::Fraction::new(opts.fps as i32, 1))
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .field("interlace-mode", "progressive")
        .build();
    let blendf = make("capsfilter", "blendcaps")?;
    blendf.set_property("caps", &blend_caps);
    let capsf = make("capsfilter", "outcaps")?;
    capsf.set_property("caps", &out_caps);
    let vconv = make("videoconvert", "vconv")?;
    let venc = make("x264enc", "venc")?;
    venc.set_property("bitrate", opts.video_kbps);
    venc.set_property_from_str("speed-preset", "veryfast");
    venc.set_property("key-int-max", opts.fps * 2);
    venc.set_property("threads", 0u32);
    let vparse = make("h264parse", "vparse")?;
    let vqueue = make("queue", "vqueue")?;
    let mux = make("mp4mux", "mux")?;
    mux.set_property("fragment-duration", 2000u32);
    let sink = make("filesink", "sink")?;
    sink.set_property("location", composite.to_string_lossy().to_string());
    pipeline.add_many([
        &comp, &blendf, &vconv, &capsf, &venc, &vparse, &vqueue, &mux, &sink,
    ])?;
    gst::Element::link_many([
        &comp, &blendf, &vconv, &capsf, &venc, &vparse, &vqueue, &mux, &sink,
    ])?;

    let total = gst::ClockTime::from_mseconds(timeline.duration_ms);
    let mut zorder = 0u32;

    // Background: keeps the composite going for the whole recording even with no other input.
    add_still(
        &pipeline,
        &comp,
        Image::solid(16, 9, [0, 0, 0, 255]),
        total,
        zorder,
        opts.fps,
    )?
    .set_rect(
        Rect {
            x: 0,
            y: 0,
            w: opts.width,
            h: opts.height,
        },
        1.0,
    );
    zorder += 1;

    // Speaker borders (under the tiles), then slates and video, then labels and the strip on top.
    let slots = timeline.slots();
    let mut borders: HashMap<(String, Role), StillPad> = HashMap::new();
    for (peer, role) in &slots {
        if *role == Role::Tile {
            let name = timeline.names.get(peer).cloned().unwrap_or_default();
            let colour = crate::raster::signature_colour(&name);
            let img = Image::solid(16, 9, [colour[0], colour[1], colour[2], 255]);
            borders.insert(
                (peer.clone(), *role),
                add_still(&pipeline, &comp, img, total, zorder, opts.fps)?,
            );
        }
    }
    zorder += 1;
    let mut slates: HashMap<(String, Role), StillPad> = HashMap::new();
    for (peer, role) in &slots {
        let name = timeline
            .names
            .get(peer)
            .cloned()
            .unwrap_or_else(|| "Guest".into());
        let img = font.slate(&name, 640, 360, policy.slate_initials_font_px);
        slates.insert(
            (peer.clone(), *role),
            add_still(&pipeline, &comp, img, total, zorder, opts.fps)?,
        );
    }
    zorder += 1;
    // Camera orientation over time, per track (`recorder.orientation`, `docs/layout.md`): a phone sends its sensor picture
    // and says how far to turn it.
    let mut orientations: HashMap<String, Vec<(u64, u16)>> = HashMap::new();
    for e in events.iter().filter(|e| e.kind == "recorder.orientation") {
        if let (Some(track), Some(rotation)) = (
            e.fields.get("trackId").and_then(|v| v.as_str()),
            e.fields.get("rotation").and_then(|v| v.as_u64()),
        ) {
            orientations
                .entry(track.to_string())
                .or_default()
                .push((e.t, rotation as u16));
        }
    }
    let orientations_all = orientations.clone();
    let mut sizes: HashMap<String, (u32, u32)> = HashMap::new();
    let mut videos: HashMap<String, VideoPad> = HashMap::new();
    for track in present.iter().filter(|t| t.kind != TrackKind::Mic) {
        if let Some(size) = probe_video_size(&dir.join(&track.file)) {
            sizes.insert(track.track_id.clone(), size);
        }
        let t0 = offsets
            .get(&track.track_id)
            .copied()
            .unwrap_or(track.start_t as i64);
        let pad = add_video(
            &pipeline,
            &comp,
            &dir.join(&track.file),
            &track.codec,
            t0,
            duration_ms,
            opts.fps,
            zorder,
            orientations.remove(&track.track_id).unwrap_or_default(),
        )?;
        videos.insert(track.track_id.clone(), pad);
    }
    zorder += 1;
    // Rounded tile corners: four small black stills with a transparent quarter disc, per (peer, role), over
    // everything the tile shows; the radius is fixed in pixels, so the corner does not scale with the tile.
    let radius = policy.tile_corner_radius_px;
    let mut corners: HashMap<(String, Role), Vec<StillPad>> = HashMap::new();
    if radius > 0 {
        for (peer, role) in &slots {
            let mut four = Vec::with_capacity(4);
            for which in 0..4 {
                let img = crate::raster::corner(radius, which);
                four.push(add_still(
                    &pipeline,
                    &comp,
                    img,
                    total,
                    zorder + 1,
                    opts.fps,
                )?);
            }
            corners.insert((peer.clone(), *role), four);
        }
    }
    // Labels: one per (peer, role); the text can change (muted, hand), so one image per distinct text.
    let mut labels: HashMap<(String, Role, String), StillPad> = HashMap::new();
    let mut strips: HashMap<String, StillPad> = HashMap::new();
    for s in &timeline.scenes {
        for tile in &s.tiles {
            let key = (tile.peer_id.clone(), tile.role, tile.label.clone());
            if let Entry::Vacant(slot) = labels.entry(key) {
                let img = font.label(
                    &tile.name,
                    &tile.status,
                    policy.label_font_px,
                    policy.label_height_px,
                    opts.width,
                );
                slot.insert(add_still(&pipeline, &comp, img, total, zorder, opts.fps)?);
            }
        }
        if !s.strip.is_empty() {
            let text = format!("Also here: {}", s.strip.join(", "));
            if let Entry::Vacant(slot) = strips.entry(text.clone()) {
                let img = font.label_text(
                    &text,
                    policy.label_font_px,
                    policy.name_strip_height_px,
                    opts.width - 2 * policy.gap_px,
                );
                slot.insert(add_still(&pipeline, &comp, img, total, zorder, opts.fps)?);
            }
        }
    }

    // The title card opens the video (`docs/layout.md`): full frame for 1 s, gone by 1.5 s, over everything.
    let participants = {
        let mut ids: std::collections::HashSet<String> =
            manifest.peers.iter().map(|p| p.peer_id.clone()).collect();
        for e in events.iter().filter(|e| e.kind == "peer.joined") {
            if let Some(id) = e.fields.get("peerId").and_then(|v| v.as_str()) {
                ids.insert(id.to_string());
            }
        }
        ids.len()
    };
    let card = font.title_card(
        manifest.title.as_deref().unwrap_or("Meeting recording"),
        &crate::raster::utc_stamp(manifest.started_at),
        manifest.started_by_name.as_deref(),
        participants,
        &opts.brand,
        opts.width,
        opts.height,
    );
    {
        let full = Rect {
            x: 0,
            y: 0,
            w: opts.width,
            h: opts.height,
        };
        let intro = add_still(
            &pipeline,
            &comp,
            Image {
                width: card.width,
                height: card.height,
                data: card.data.clone(),
            },
            total,
            zorder + 2,
            opts.fps,
        )?;
        intro.set_rect(full, 1.0);
        intro.geometry.key(1000, 0, full, 1.0);
        intro.geometry.key(1500, 0, full, 0.0);
    }

    // Geometry over time.
    let transition = policy.transition_ms;
    for (i, s) in timeline.scenes.iter().enumerate() {
        let t = s.t;
        let end = timeline
            .scenes
            .get(i + 1)
            .map(|n| n.t)
            .unwrap_or(duration_ms);
        let mut shown_videos: Vec<&str> = Vec::new();
        let mut shown_slates: Vec<(String, Role)> = Vec::new();
        let mut shown_labels: Vec<(String, Role, String)> = Vec::new();
        let mut shown_borders: Vec<(String, Role)> = Vec::new();
        let mut shown_corners: Vec<(String, Role)> = Vec::new();
        for tile in &s.tiles {
            // The picture's real shape at this moment, fitted and centred in the tile: a portrait phone gets a
            // portrait tile, and the label, corners and border sit on the picture, not on the bars beside it.
            let r = match &tile.content {
                Content::Video { track_id } => match sizes.get(track_id) {
                    Some(&(w, h)) if w > 0 && h > 0 => {
                        let rotation = orientations_all
                            .get(track_id)
                            .and_then(|list| list.iter().rev().find(|(at, _)| *at <= t + 200))
                            .map(|(_, r)| *r)
                            .unwrap_or(0);
                        let aspect = if rotation == 90 || rotation == 270 {
                            h as f64 / w as f64
                        } else {
                            w as f64 / h as f64
                        };
                        fitted(tile.rect, aspect)
                    }
                    _ => tile.rect,
                },
                _ => tile.rect,
            };
            if let Some(four) = corners.get(&(tile.peer_id.clone(), tile.role)) {
                let d = radius as i32;
                let spots = [
                    (r.x, r.y),
                    (r.x + r.w as i32 - d, r.y),
                    (r.x, r.y + r.h as i32 - d),
                    (r.x + r.w as i32 - d, r.y + r.h as i32 - d),
                ];
                for (c, (x, y)) in four.iter().zip(spots) {
                    c.geometry.key(
                        t,
                        transition,
                        Rect {
                            x,
                            y,
                            w: radius,
                            h: radius,
                        },
                        1.0,
                    );
                }
                shown_corners.push((tile.peer_id.clone(), tile.role));
            }
            match &tile.content {
                Content::Video { track_id } => {
                    if let Some(v) = videos.get(track_id) {
                        v.geometry.key(t, transition, r, 1.0);
                        shown_videos.push(track_id);
                    }
                }
                Content::Slate => {}
            }
            if let Some(sl) = slates.get(&(tile.peer_id.clone(), tile.role)) {
                let visible = matches!(tile.content, Content::Slate);
                sl.geometry
                    .key(t, transition, r, if visible { 1.0 } else { 0.0 });
                if visible {
                    shown_slates.push((tile.peer_id.clone(), tile.role));
                }
            }
            let key = (tile.peer_id.clone(), tile.role, tile.label.clone());
            if let Some(l) = labels.get(&key) {
                let w = l.width.min(r.w);
                l.geometry.key(
                    t,
                    transition,
                    Rect {
                        x: r.x,
                        y: r.y + r.h as i32 - l.height as i32,
                        w,
                        h: l.height,
                    },
                    1.0,
                );
                shown_labels.push(key);
            }
            if tile.role == Role::Tile {
                if let Some(b) = borders.get(&(tile.peer_id.clone(), tile.role)) {
                    let bw = policy.speaker_border_px as i32;
                    b.geometry.key(
                        t,
                        transition,
                        Rect {
                            x: r.x - bw,
                            y: r.y - bw,
                            w: r.w + 2 * bw as u32,
                            h: r.h + 2 * bw as u32,
                        },
                        if tile.speaker { 1.0 } else { 0.0 },
                    );
                    if tile.speaker {
                        shown_borders.push((tile.peer_id.clone(), tile.role));
                    }
                }
            }
        }
        for (id, v) in &videos {
            if !shown_videos.contains(&id.as_str()) {
                v.geometry.hide(t);
            }
        }
        for (key, sl) in &slates {
            if !shown_slates.contains(key) {
                sl.geometry.hide(t);
            }
        }
        for (key, l) in &labels {
            if !shown_labels.contains(key) {
                l.geometry.hide(t);
            }
        }
        for (key, four) in &corners {
            if !shown_corners.contains(key) {
                for c in four {
                    c.geometry.hide(t);
                }
            }
        }
        for (key, b) in &borders {
            if !shown_borders.contains(key) {
                b.geometry.hide(t);
            }
        }
        let strip_text = if s.strip.is_empty() {
            None
        } else {
            Some(format!("Also here: {}", s.strip.join(", ")))
        };
        for (text, st) in &strips {
            if strip_text.as_deref() == Some(text.as_str()) {
                let y =
                    opts.height as i32 - policy.gap_px as i32 - policy.name_strip_height_px as i32;
                st.geometry.key(
                    t,
                    0,
                    Rect {
                        x: policy.gap_px as i32,
                        y,
                        w: st.width,
                        h: st.height,
                    },
                    1.0,
                );
            } else {
                st.geometry.hide(t);
            }
        }
        let _ = end;
    }

    // Audio.
    let mut audio_out: Option<PathBuf> = None;
    if has_audio {
        let amix = make("audiomixer", "amix")?;
        let aconv = make("audioconvert", "aconv")?;
        let ares = make("audioresample", "ares")?;
        let acaps = make("capsfilter", "acaps")?;
        acaps.set_property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("rate", 48000i32)
                .field("channels", 2i32)
                .field("layout", "interleaved")
                .build(),
        );
        let aenc = make("avenc_aac", "aenc")?;
        aenc.set_property("bitrate", (opts.audio_kbps * 1000) as i32);
        let aparse = make("aacparse", "aparse")?;
        let tee = make("tee", "atee")?;
        let aq1 = make("queue", "aq1")?;
        let aq2 = make("queue", "aq2")?;
        let mux2 = make("mp4mux", "mux2")?;
        let sink2 = make("filesink", "sink2")?;
        sink2.set_property("location", audio_mix.to_string_lossy().to_string());
        pipeline.add_many([
            &amix, &aconv, &ares, &acaps, &aenc, &aparse, &tee, &aq1, &aq2, &mux2, &sink2,
        ])?;
        gst::Element::link_many([&amix, &acaps, &aconv, &ares, &aenc, &aparse, &tee])?;
        tee.link(&aq1)?;
        aq1.link(&mux)?;
        tee.link(&aq2)?;
        gst::Element::link_many([&aq2, &mux2, &sink2])?;
        for track in present.iter().filter(|t| t.kind == TrackKind::Mic) {
            let t0 = offsets
                .get(&track.track_id)
                .copied()
                .unwrap_or(track.start_t as i64);
            add_audio(&pipeline, &amix, &dir.join(&track.file), t0, duration_ms)?;
        }
        audio_out = Some(audio_mix.clone());
    }

    run(
        &pipeline,
        duration_ms,
        &manifest.recording_id,
        opts.hold.as_deref(),
    )?;

    // The thumbnail is the same title card.
    {
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", card.width as i32)
            .field("height", card.height as i32)
            .field("framerate", gst::Fraction::new(1, 1))
            .build();
        let buf = gst::Buffer::from_mut_slice(card.data);
        write_jpeg(&buf, &caps, &thumbnail)?;
    }

    let bytes = std::fs::metadata(&composite).map(|m| m.len()).unwrap_or(0);
    if bytes == 0 {
        bail!("composite.mp4 is empty");
    }
    manifest.composite = Some(CompositeInfo {
        file: "composite.mp4".into(),
        codec: "h264/aac".into(),
        width: opts.width,
        height: opts.height,
        fps: opts.fps,
        bytes,
        duration_ms,
        policy: policy.name.clone(),
        policy_version: policy.version,
        composed_at: now_ms(),
        missing,
    });
    manifest.thumbnail = Some(ThumbnailInfo {
        file: "thumbnail.jpg".into(),
        at_t: 0,
    });
    manifest.audio_mix = audio_out.as_ref().map(|_| AudioMixInfo {
        file: "audio.m4a".into(),
    });
    tracing::info!(recording = %manifest.recording_id, bytes, ms = started.elapsed().as_millis() as u64,
        speed = format!("{:.1}x", duration_ms as f64 / started.elapsed().as_millis().max(1) as f64), "composed");
    Ok(ComposeResult {
        manifest,
        composite,
        thumbnail,
        audio_mix: audio_out,
    })
}

/// Where each track file's time 0 sits on the recording clock (ms), from the sender reports (`docs/spool-format.md`):
/// NTP of `firstRtpTs` per track through its own SRs, NTP → recording clock through all SRs of all tracks
/// (one SFU clock). A track without usable reports falls back to its first packet's arrival.
pub fn alignment(tracks: &[TrackManifest]) -> HashMap<String, i64> {
    let mut k_samples: Vec<f64> = tracks
        .iter()
        .flat_map(|t| {
            t.sender_reports
                .iter()
                .map(|sr| sr.t as f64 - sr.ntp_s * 1000.0)
        })
        .collect();
    let k = median(&mut k_samples);
    let mut out = HashMap::new();
    for t in tracks {
        let fallback = t.start_t as i64;
        let mut ntp0: Vec<f64> = t
            .sender_reports
            .iter()
            .map(|sr| {
                let diff = t.first_rtp_ts.wrapping_sub(sr.rtp_ts) as i32 as f64;
                sr.ntp_s * 1000.0 + diff / t.clock_rate.max(1) as f64 * 1000.0
            })
            .collect();
        let t0 = match (median(&mut ntp0), k) {
            (Some(n), Some(k)) => {
                let t0 = (n + k).round() as i64;
                if (t0 - fallback).abs() <= 3000 {
                    t0
                } else {
                    tracing::warn!(track = %t.track_id, t0, fallback, "sender-report alignment implausible; using arrival");
                    fallback
                }
            }
            _ => fallback,
        };
        out.insert(t.track_id.clone(), t0.max(0));
    }
    out
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("GStreamer element {factory} is not available"))
}

/// The controllable geometry of one compositor pad.
struct Geometry {
    x: gst_controller::InterpolationControlSource,
    y: gst_controller::InterpolationControlSource,
    w: gst_controller::InterpolationControlSource,
    h: gst_controller::InterpolationControlSource,
    alpha: gst_controller::InterpolationControlSource,
    last: Mutex<Option<(Rect, f64)>>,
    keys: Mutex<BTreeMap<u64, (Rect, f64)>>,
}

impl Geometry {
    fn bind(pad: &gst::Pad) -> Result<Self> {
        let cs = |mode| {
            let c = gst_controller::InterpolationControlSource::new();
            c.set_mode(mode);
            c
        };
        let g = Self {
            x: cs(gst_controller::InterpolationMode::Linear),
            y: cs(gst_controller::InterpolationMode::Linear),
            w: cs(gst_controller::InterpolationMode::Linear),
            h: cs(gst_controller::InterpolationMode::Linear),
            alpha: cs(gst_controller::InterpolationMode::None),
            last: Mutex::new(None),
            keys: Mutex::new(BTreeMap::new()),
        };
        for (prop, src) in [
            ("xpos", &g.x),
            ("ypos", &g.y),
            ("width", &g.w),
            ("height", &g.h),
            ("alpha", &g.alpha),
        ] {
            let binding = gst_controller::DirectControlBinding::new_absolute(pad, prop, src);
            pad.add_control_binding(&binding)
                .map_err(|_| anyhow!("control binding for {prop}"))?;
        }
        Ok(g)
    }

    /// Set the geometry at `t`, sliding from the previous one over `transition_ms` (ending at `t`).
    fn key(&self, t: u64, transition_ms: u64, r: Rect, alpha: f64) {
        let mut last = self.last.lock().unwrap();
        if let Some((prev, prev_alpha)) = *last {
            if prev_alpha > 0.0 && alpha > 0.0 && transition_ms > 0 && t >= transition_ms {
                let from = t - transition_ms;
                self.x.set(ms(from), prev.x as f64);
                self.y.set(ms(from), prev.y as f64);
                self.w.set(ms(from), prev.w as f64);
                self.h.set(ms(from), prev.h as f64);
            }
        }
        self.x.set(ms(t), r.x as f64);
        self.y.set(ms(t), r.y as f64);
        self.w.set(ms(t), r.w.max(1) as f64);
        self.h.set(ms(t), r.h.max(1) as f64);
        self.alpha.set(ms(t), alpha);
        *last = Some((r, alpha));
        self.keys.lock().unwrap().insert(t, (r, alpha));
    }

    fn hide(&self, t: u64) {
        let last = *self.last.lock().unwrap();
        if let Some((r, a)) = last {
            if a == 0.0 {
                return;
            }
            self.key(t, 0, r, 0.0);
        } else {
            self.alpha.set(ms(t), 0.0);
        }
    }

    fn set_rect(&self, r: Rect, alpha: f64) {
        self.key(0, 0, r, alpha);
    }
}

fn ms(t: u64) -> gst::ClockTime {
    gst::ClockTime::from_mseconds(t)
}

struct StillPad {
    geometry: Geometry,
    width: u32,
    height: u32,
}

impl StillPad {
    fn set_rect(&self, r: Rect, alpha: f64) {
        self.geometry.set_rect(r, alpha);
    }
}

struct VideoPad {
    geometry: Geometry,
}

/// One RGBA image as a compositor input for the whole recording (a single long buffer, then EOS).
fn add_still(
    pipeline: &gst::Pipeline,
    comp: &gst::Element,
    img: Image,
    total: gst::ClockTime,
    zorder: u32,
    fps: u32,
) -> Result<StillPad> {
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .field("width", img.width as i32)
        .field("height", img.height as i32)
        .field("framerate", gst::Fraction::new(fps as i32, 1))
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .build();
    let src = gst_app::AppSrc::builder()
        .caps(&caps)
        .format(gst::Format::Time)
        .build();
    let conv = make(
        "videoconvert",
        &format!("stillconv{zorder}-{}", rand_name()),
    )?;
    pipeline.add_many([src.upcast_ref::<gst::Element>(), &conv])?;
    src.link(&conv)?;
    let pad = comp
        .request_pad_simple("sink_%u")
        .ok_or_else(|| anyhow!("compositor pad"))?;
    conv.static_pad("src").unwrap().link(&pad)?;
    pad.set_property("zorder", zorder);
    pad.set_property_from_str("sizing-policy", "none");
    pad.set_property("alpha", 0.0f64);
    let mut buffer = gst::Buffer::from_mut_slice(img.data);
    {
        let b = buffer.get_mut().unwrap();
        b.set_pts(gst::ClockTime::ZERO);
        b.set_dts(gst::ClockTime::ZERO);
        b.set_duration(total);
    }
    src.push_buffer(buffer)
        .map_err(|e| anyhow!("still push: {e:?}"))?;
    src.end_of_stream()
        .map_err(|e| anyhow!("still eos: {e:?}"))?;
    Ok(StillPad {
        geometry: Geometry::bind(&pad)?,
        width: img.width,
        height: img.height,
    })
}

/// Drops every buffer of an input whose place on the recording clock is past its end: a track file with a
/// timestamp jump must not drag the compositor along for hours.
fn clamp_to_recording(pad: &gst::Pad, t0_ms: i64, total_ms: u64) {
    let limit_ns = (total_ms as i64 + 1_000 - t0_ms).max(0) as u64 * 1_000_000;
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(buf) = info.buffer_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(pts) = buf.pts() else {
            return gst::PadProbeReturn::Ok;
        };
        if pts.nseconds() > limit_ns {
            return gst::PadProbeReturn::Drop;
        }
        // The frame before a timestamp jump carries the jump as its duration; the aggregator would render it
        // for that long.
        if let Some(d) = buf.duration() {
            if pts.nseconds() + d.nseconds() > limit_ns {
                buf.make_mut()
                    .set_duration(gst::ClockTime::from_nseconds(limit_ns - pts.nseconds()));
            }
        }
        gst::PadProbeReturn::Ok
    });
}

// One input per argument reads better than a struct that exists for a single call site.
#[allow(clippy::too_many_arguments)]
fn add_video(
    pipeline: &gst::Pipeline,
    comp: &gst::Element,
    file: &Path,
    codec: &str,
    t0_ms: i64,
    total_ms: u64,
    fps: u32,
    zorder: u32,
    orientation: Vec<(u64, u16)>,
) -> Result<VideoPad> {
    let id = rand_name();
    let src = make("filesrc", &format!("vsrc-{id}"))?;
    src.set_property("location", file.to_string_lossy().to_string());
    let demux = make("matroskademux", &format!("vdemux-{id}"))?;
    let (parse, dec) = match codec.to_ascii_lowercase().as_str() {
        "video/vp8" => (None, make("vp8dec", &format!("vdec-{id}"))?),
        "video/vp9" => (None, make("vp9dec", &format!("vdec-{id}"))?),
        "video/h264" => (
            Some(make("h264parse", &format!("vparse-{id}"))?),
            make("avdec_h264", &format!("vdec-{id}"))?,
        ),
        other => bail!("codec {other} cannot be composed"),
    };
    let conv = make("videoconvert", &format!("vconv-{id}"))?;
    // Turns the picture as the sender's orientation says, following the changes over the recording clock.
    let flip = make("videoflip", &format!("vflip-{id}"))?;
    // Rotate in BGRA: videoflip on I420 halves the brightness of a rotated frame whose chroma line count comes
    // out odd (GStreamer 1.26), and the compositor blends in BGRA anyway.
    let flip_caps = make("capsfilter", &format!("vfcaps-{id}"))?;
    flip_caps.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .build(),
    );
    {
        let method_at = move |t: u64| -> &'static str {
            let rotation = orientation
                .iter()
                .rev()
                .find(|(at, _)| *at <= t + 200)
                .map(|(_, r)| *r)
                .unwrap_or(0);
            match rotation {
                90 => "clockwise",
                180 => "rotate-180",
                270 => "counterclockwise",
                _ => "none",
            }
        };
        let initial = method_at(t0_ms.max(0) as u64);
        flip.set_property_from_str("method", initial);
        let current = Arc::new(Mutex::new(initial));
        let flip_for_probe = flip.clone();
        flip.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                    if let Some(pts) = b.pts() {
                        let t = (t0_ms + pts.mseconds() as i64).max(0) as u64;
                        let wanted = method_at(t);
                        let mut cur = current.lock().unwrap();
                        if *cur != wanted {
                            *cur = wanted;
                            flip_for_probe.set_property_from_str("method", wanted);
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
    }
    // A gap in the track (loss, a frame dropped while waiting for a keyframe) must not show the background
    // through the tile: repeat the last good frame at the output rate until the next one arrives.
    let rate = make("videorate", &format!("vrate-{id}"))?;
    rate.set_property("skip-to-first", true);
    let rate_caps = make("capsfilter", &format!("vrcaps-{id}"))?;
    rate_caps.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("framerate", gst::Fraction::new(fps as i32, 1))
            .build(),
    );
    let queue = make("queue", &format!("vq-{id}"))?;
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 3_000_000_000u64);
    pipeline.add_many([
        &src, &demux, &conv, &flip_caps, &flip, &rate, &rate_caps, &queue,
    ])?;
    pipeline.add(&dec)?;
    src.link(&demux)?;
    let first: gst::Element = match &parse {
        Some(p) => {
            pipeline.add(p)?;
            p.link(&dec)?;
            p.clone()
        }
        None => dec.clone(),
    };
    gst::Element::link_many([&dec, &conv, &flip_caps, &flip, &rate, &rate_caps, &queue])?;
    demux.connect_pad_added(move |_, pad| {
        if let Some(sink) = first.static_pad("sink") {
            if !sink.is_linked() {
                if let Err(e) = pad.link(&sink) {
                    tracing::warn!(error = ?e, "video demux pad link");
                }
            }
        }
    });
    let cpad = comp
        .request_pad_simple("sink_%u")
        .ok_or_else(|| anyhow!("compositor pad"))?;
    clamp_to_recording(&queue.static_pad("src").unwrap(), t0_ms, total_ms);
    queue.static_pad("src").unwrap().link(&cpad)?;
    cpad.set_property("zorder", zorder);
    cpad.set_property_from_str("sizing-policy", "keep-aspect-ratio");
    cpad.set_property("alpha", 0.0f64);
    cpad.set_offset(t0_ms.max(0) * 1_000_000);
    Ok(VideoPad {
        geometry: Geometry::bind(&cpad)?,
    })
}

fn add_audio(
    pipeline: &gst::Pipeline,
    amix: &gst::Element,
    file: &Path,
    t0_ms: i64,
    total_ms: u64,
) -> Result<()> {
    let id = rand_name();
    let src = make("filesrc", &format!("asrc-{id}"))?;
    src.set_property("location", file.to_string_lossy().to_string());
    let demux = make("matroskademux", &format!("ademux-{id}"))?;
    let dec = make("opusdec", &format!("adec-{id}"))?;
    let conv = make("audioconvert", &format!("aconv-{id}"))?;
    let res = make("audioresample", &format!("ares-{id}"))?;
    let caps = make("capsfilter", &format!("acaps-{id}"))?;
    caps.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("rate", 48000i32)
            .field("channels", 2i32)
            .field("layout", "interleaved")
            .build(),
    );
    let queue = make("queue", &format!("aq-{id}"))?;
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 3_000_000_000u64);
    pipeline.add_many([&src, &demux, &dec, &conv, &res, &caps, &queue])?;
    src.link(&demux)?;
    gst::Element::link_many([&dec, &conv, &res, &caps, &queue])?;
    let dec2 = dec.clone();
    demux.connect_pad_added(move |_, pad| {
        if let Some(sink) = dec2.static_pad("sink") {
            if !sink.is_linked() {
                if let Err(e) = pad.link(&sink) {
                    tracing::warn!(error = ?e, "audio demux pad link");
                }
            }
        }
    });
    let mpad = amix
        .request_pad_simple("sink_%u")
        .ok_or_else(|| anyhow!("audiomixer pad"))?;
    clamp_to_recording(&queue.static_pad("src").unwrap(), t0_ms, total_ms);
    queue.static_pad("src").unwrap().link(&mpad)?;
    mpad.set_offset(t0_ms.max(0) * 1_000_000);
    Ok(())
}

/// PLAYING → EOS, with progress every few seconds; any error message aborts.
fn run(
    pipeline: &gst::Pipeline,
    duration_ms: u64,
    id: &str,
    hold: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> Result<()> {
    pipeline
        .set_state(gst::State::Playing)
        .context("pipeline to PLAYING")?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
    let started = Instant::now();
    let mut last_log = Instant::now();
    let mut forced_eos = false;
    let mut held = false;
    let result = loop {
        match bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(2),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(_) => break Ok(()),
                gst::MessageView::Error(e) => {
                    break Err(anyhow!(
                        "{} ({})",
                        e.error(),
                        e.debug().map(|d| d.to_string()).unwrap_or_default()
                    ))
                }
                _ => {}
            },
            None => {
                // A capture started on this box: the render waits (its encoder threads would starve the capture).
                if let Some(hold) = hold {
                    let wants_hold = hold();
                    if wants_hold && !held {
                        tracing::info!(recording = %id, "compose paused: a capture is live");
                        let _ = pipeline.set_state(gst::State::Paused);
                        held = true;
                    } else if !wants_hold && held {
                        tracing::info!(recording = %id, "compose resumed");
                        let _ = pipeline.set_state(gst::State::Playing);
                        held = false;
                    }
                    if held {
                        continue;
                    }
                }
                let pos = pipeline.query_position::<gst::ClockTime>();
                // An input whose timeline runs past the recording (a bad timestamp in a track file) would keep
                // the compositor rendering for hours: end the file where the recording ended.
                if let Some(pos) = pos {
                    if !forced_eos && pos.mseconds() > duration_ms + 15_000 {
                        forced_eos = true;
                        tracing::warn!(recording = %id, position_ms = pos.mseconds(), duration_ms, "input ran past the recording; forcing EOS");
                        pipeline.send_event(gst::event::Eos::new());
                    }
                }
                if last_log.elapsed().as_secs() >= 10 {
                    last_log = Instant::now();
                    if let Some(pos) = pos {
                        let pct = pos.mseconds() as f64 / duration_ms.max(1) as f64 * 100.0;
                        tracing::info!(recording = %id, pct = format!("{pct:.0}"), elapsed_s = started.elapsed().as_secs(), "composing");
                    }
                }
            }
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    result
}

fn write_jpeg(frame: &gst::Buffer, caps: &gst::Caps, out: &Path) -> Result<()> {
    let pipeline = gst::Pipeline::with_name("thumbnail");
    let src = gst_app::AppSrc::builder()
        .caps(caps)
        .format(gst::Format::Time)
        .build();
    let conv = make("videoconvert", "tconv")?;
    let enc = make("jpegenc", "tenc")?;
    enc.set_property("quality", 85i32);
    let sink = make("filesink", "tsink")?;
    sink.set_property("location", out.to_string_lossy().to_string());
    pipeline.add_many([src.upcast_ref::<gst::Element>(), &conv, &enc, &sink])?;
    gst::Element::link_many([src.upcast_ref::<gst::Element>(), &conv, &enc, &sink])?;
    let mut buf = frame.copy();
    {
        let b = buf.get_mut().unwrap();
        b.set_pts(gst::ClockTime::ZERO);
        b.set_dts(gst::ClockTime::ZERO);
    }
    pipeline.set_state(gst::State::Playing)?;
    src.push_buffer(buf)
        .map_err(|e| anyhow!("thumbnail push: {e:?}"))?;
    src.end_of_stream()
        .map_err(|e| anyhow!("thumbnail eos: {e:?}"))?;
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => Ok(()),
        Some(gst::MessageView::Error(e)) => Err(anyhow!("thumbnail: {}", e.error())),
        _ => Err(anyhow!("thumbnail: timeout")),
    }
}

/// The largest rectangle of the given aspect (width over height) inside `r`, centred.
fn fitted(r: Rect, aspect: f64) -> Rect {
    let (w, h) = if (r.w as f64) / (r.h as f64) > aspect {
        (((r.h as f64) * aspect).round() as u32, r.h)
    } else {
        (r.w, ((r.w as f64) / aspect).round() as u32)
    };
    Rect {
        x: r.x + (r.w as i32 - w as i32) / 2,
        y: r.y + (r.h as i32 - h as i32) / 2,
        w: w.max(1),
        h: h.max(1),
    }
}

/// Width and height of the first video stream of a WebM file, from its header (no decoding).
fn probe_video_size(path: &Path) -> Option<(u32, u32)> {
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! matroskademux name=d ! fakesink sync=false",
        path.display()
    ))
    .ok()?;
    let bin = pipeline.downcast_ref::<gst::Bin>()?;
    let demux = bin.by_name("d")?;
    pipeline.set_state(gst::State::Paused).ok()?;
    let bus = pipeline.bus()?;
    let _ = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::AsyncDone, gst::MessageType::Error],
    );
    let mut size = None;
    for pad in demux.src_pads() {
        if let Some(caps) = pad.current_caps() {
            if let Some(s) = caps.structure(0) {
                if s.name().starts_with("video/") {
                    if let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) {
                        size = Some((w as u32, h as u32));
                    }
                }
            }
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    size
}

fn rand_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}", N.fetch_add(1, Ordering::Relaxed))
}

#[allow(dead_code)]
fn _timeline_debug(tl: &Timeline) -> String {
    tl.scenes
        .iter()
        .map(|s| format!("{}: {} tiles", s.t, s.tiles.len()))
        .collect::<Vec<_>>()
        .join("\n")
}
