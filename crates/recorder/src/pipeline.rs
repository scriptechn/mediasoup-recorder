//! One GStreamer pipeline per track (`docs/architecture.md`):
//!
//! ```text
//! udpsrc(rtp) ──▶ rtpbin(avpf, do-retransmission) ──▶ depay ──▶ [pts = (rtp - first)/clock] ──▶ matroskamux ──▶ filesink
//! udpsrc(rtcp) ─▶ rtpbin ; rtpbin.send_rtcp_src ──▶ udpsink(rtcp, SAME socket as udpsrc(rtcp))
//! ```
//!
//! Facts this file depends on, each one measured against mediasoup before it was written down:
//! - our RTCP must leave from the socket we receive RTCP on, or the SFU drops it (mediasoup with `comedia: false` checks the tuple);
//! - payload types come from the consumer's rtpParameters, never from a preference of ours;
//! - the file timeline is the RTP timestamp; rtpbin's own clock-skew estimate never reaches the file;
//! - every Sender Report we receive is kept (NTP ↔ RTP) so compose can align tracks.

use crate::spool::{Gap, SenderReportSample};
use crate::wire::{AllocateTrack, TrackKind};
use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Live counters, shared with the session; read at stop into the manifest.
#[derive(Debug, Default)]
pub struct TrackStats {
    pub first_rtp_ts: Option<u32>,
    /// `t` of the first frame written to the file: for video the first keyframe, which can come well after the
    /// first packet. The manifest's `startT` is this, so the composite shows the slate until then.
    pub first_frame_t: Option<u64>,
    /// Frames (or audio packets) written to the file so far.
    pub frames_written: u64,
    /// Key frames seen at the socket (VP8 only: the other codecs are not parsed and leave this at zero). The
    /// keyframe chase reads it, since a frame reaches the file only a jitter-buffer latency after it arrived.
    pub keyframes_in: u64,
    pub packets: u64,
    pub packets_lost: u64,
    pub bytes: u64,
    pub start_t: Option<u64>,
    pub end_t: u64,
    pub sender_reports: Vec<SenderReportSample>,
    pub gaps: Vec<Gap>,
}

/// What the pipeline reports back to the session while running.
pub enum PipelineEvent {
    /// A loss the jitter buffer gave up on; the session asks the controller for a keyframe.
    Gap {
        from_t: u64,
        to_t: u64,
        lost_packets: u64,
    },
    /// The sender's camera orientation changed (`urn:3gpp:video-orientation`): degrees clockwise the picture
    /// must be turned to be upright, and whether it is mirrored.
    Orientation { t: u64, rotation: u16, flip: bool },
    /// GStreamer error: the track is over.
    Error(String),
}

pub struct TrackPipeline {
    pipeline: gst::Pipeline,
    pub stats: Arc<Mutex<TrackStats>>,
    rtcp_sink: gst::Element,
    /// Set before EOS: the flush makes the jitter buffer report the packets it was still waiting for as lost.
    stopping: Arc<AtomicBool>,
    /// Set while the producer is paused (mute, camera off without closing): nothing arrives on purpose, and the
    /// packet the jitter buffer then waits for is not a loss.
    quiet: Arc<AtomicBool>,
    /// The jitter buffers rtpbin created (one per SSRC), for the stop-time property change.
    jitterbuffers: Arc<Mutex<Vec<gst::Element>>>,
}

/// The file-timeline state of one track: first rtp ts, unwrapped ticks since it, last seq, last arrival t (ms).
type PtsState = Arc<Mutex<Option<(u32, i64, u16, u64)>>>;

pub struct TrackPipelineOptions<'a> {
    pub track: &'a AllocateTrack,
    pub rtp_port: u16,
    pub rtcp_port: u16,
    pub bind_ip: &'a str,
    pub file: &'a Path,
    pub jitter_buffer_ms: u32,
    /// `t` clock of the recording, so stats line up with the event log.
    pub started: Instant,
    pub on_event: Box<dyn Fn(PipelineEvent) + Send + Sync + 'static>,
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("missing GStreamer element '{factory}'"))
}

/// Depayloader and caps for the negotiated codec (the consumer's first codec; RTX is the second one, if any).
fn codec_elements(track: &AllocateTrack) -> Result<(&'static str, gst::Caps)> {
    let codec = track
        .rtp_parameters
        .codecs
        .first()
        .ok_or_else(|| anyhow!("track {} has no codec", track.track_id))?;
    let pt = codec.payload_type as i32;
    let mime = codec.mime_type.to_ascii_lowercase();
    let (depay, encoding, media) = match mime.as_str() {
        "video/vp8" => ("rtpvp8depay", "VP8", "video"),
        "video/vp9" => ("rtpvp9depay", "VP9", "video"),
        "video/h264" => ("rtph264depay", "H264", "video"),
        "audio/opus" => ("rtpopusdepay", "OPUS", "audio"),
        other => return Err(anyhow!("codec {other} is not recordable")),
    };
    let mut caps = gst::Caps::builder("application/x-rtp")
        .field("media", media)
        .field("clock-rate", codec.clock_rate as i32)
        .field("encoding-name", encoding)
        .field("payload", pt)
        .field("rtcp-fb-nack", true);
    if media == "video" {
        caps = caps
            .field("rtcp-fb-nack-pli", true)
            .field("rtcp-fb-ccm-fir", true);
    }
    if mime == "audio/opus" {
        // Opus over RTP is always signalled as 2 channels (RFC 7587).
        caps = caps.field("encoding-params", "2");
    }
    Ok((depay, caps.build()))
}

impl TrackPipeline {
    /// Build and set to PLAYING. Sends nothing until `connect_rtcp` names the SFU's RTCP port.
    pub fn start(opts: TrackPipelineOptions<'_>) -> Result<Self> {
        let TrackPipelineOptions {
            track,
            rtp_port,
            rtcp_port,
            bind_ip,
            file,
            jitter_buffer_ms,
            started,
            on_event,
        } = opts;
        let on_event = Arc::new(on_event);
        let (depay_name, caps) = codec_elements(track)?;
        let cvo_id: Option<u8> = track
            .rtp_parameters
            .header_extensions
            .iter()
            .find(|e| e.uri == "urn:3gpp:video-orientation")
            .map(|e| e.id);
        let clock_rate = track.clock_rate as u128;
        let is_video = matches!(track.kind, TrackKind::Webcam | TrackKind::Screen);
        let stats = Arc::new(Mutex::new(TrackStats::default()));
        let stopping = Arc::new(AtomicBool::new(false));
        let quiet = Arc::new(AtomicBool::new(false));
        let jitterbuffers: Arc<Mutex<Vec<gst::Element>>> = Arc::new(Mutex::new(Vec::new()));

        let pipeline = gst::Pipeline::with_name(&format!("track-{}", track.track_id));
        let rtpbin = make("rtpbin", "rtpbin")?;
        rtpbin.set_property_from_str("rtp-profile", "avpf");
        rtpbin.set_property("do-retransmission", true);
        rtpbin.set_property("do-lost", true);
        rtpbin.set_property("latency", jitter_buffer_ms);
        rtpbin.set_property("drop-on-latency", false);
        pipeline.add(&rtpbin)?;

        // Caps for our one payload type, asked for by rtpbin when it demuxes.
        {
            let caps = caps.clone();
            let pt = track.rtp_parameters.codecs[0].payload_type as u32;
            rtpbin.connect("request-pt-map", false, move |args| {
                let asked: u32 = args[2].get().ok()?;
                let out: Option<gst::Caps> = if asked == pt {
                    Some(caps.clone())
                } else {
                    None
                };
                Some(out.to_value())
            });
        }

        let rtp_src = make("udpsrc", "rtp-src")?;
        rtp_src.set_property("address", bind_ip);
        rtp_src.set_property("port", rtp_port as i32);
        // A port still held by another socket is a bug to see, not to share silently (SO_REUSEADDR is the default).
        rtp_src.set_property("reuse", false);
        rtp_src.set_property("caps", &caps);
        // The kernel buffers what a busy moment keeps us from reading (the default ~200 KB is a tenth of a second
        // of video); 4 MiB is seconds, at no cost when idle.
        rtp_src.set_property("buffer-size", 4 * 1024 * 1024i32);
        // Arrival time of every packet by sequence number (ms since the recording started, plus one so that zero
        // means unknown). Track times are read back from it after the jitter buffer, which delays every packet by
        // its latency and flushes what it still holds at stop; so `t` is when the SFU sent the packet.
        let arrivals: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(vec![0u32; 65536]));
        {
            let arrivals = arrivals.clone();
            let stats = stats.clone();
            let trace = trace_rtp();
            let track_id = track.track_id.clone();
            let vp8 = depay_name == "rtpvp8depay";
            rtp_src.static_pad("src").unwrap().add_probe(
                gst::PadProbeType::BUFFER,
                move |_, info| {
                    if let Some(buf) = info.buffer() {
                        if let Ok(map) = buf.map_readable() {
                            let d = map.as_slice();
                            if d.len() >= 12 {
                                let seq = u16::from_be_bytes([d[2], d[3]]);
                                arrivals.lock().unwrap()[seq as usize] =
                                    started.elapsed().as_millis() as u32 + 1;
                                if vp8 && vp8_frame_bits(&d[rtp_payload_offset(d)..]).1 {
                                    stats.lock().unwrap().keyframes_in += 1;
                                }
                            }
                            if trace {
                                trace_packet("socket", &track_id, started, d, vp8);
                            }
                        }
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        }
        let rtcp_src = make("udpsrc", "rtcp-src")?;
        rtcp_src.set_property("address", bind_ip);
        rtcp_src.set_property("port", rtcp_port as i32);
        rtcp_src.set_property("reuse", false);
        let rtcp_sink = make("udpsink", "rtcp-sink")?;
        rtcp_sink.set_property("sync", false);
        rtcp_sink.set_property("async", false);
        // Until connect_rtcp: a harmless target; nothing is sent before rtpbin has a source anyway.
        rtcp_sink.set_property("host", "127.0.0.1");
        rtcp_sink.set_property("port", 9i32);
        pipeline.add_many([&rtp_src, &rtcp_src, &rtcp_sink])?;

        // Our RTCP leaves from the very socket we receive RTCP on (see module doc).
        rtcp_src.set_state(gst::State::Ready)?;
        let socket = rtcp_src.property_value("used-socket");
        if socket
            .get::<Option<glib::Object>>()
            .ok()
            .flatten()
            .is_none()
        {
            return Err(anyhow!(
                "udpsrc for RTCP port {rtcp_port} has no socket after READY"
            ));
        }
        rtcp_sink.set_property_from_value("socket", &socket);
        rtcp_sink.set_property("close-socket", false);

        // Sender Report log: every SR block from the SFU, with our arrival time.
        {
            let stats = stats.clone();
            rtcp_src.static_pad("src").unwrap().add_probe(
                gst::PadProbeType::BUFFER,
                move |_, info| {
                    if let Some(buf) = info.buffer() {
                        if let Ok(map) = buf.map_readable() {
                            for (ntp_s, rtp_ts) in parse_sender_reports(map.as_slice()) {
                                stats
                                    .lock()
                                    .unwrap()
                                    .sender_reports
                                    .push(SenderReportSample {
                                        t: started.elapsed().as_millis() as u64,
                                        ntp_s,
                                        rtp_ts,
                                    });
                            }
                        }
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        }

        rtp_src
            .static_pad("src")
            .unwrap()
            .link(&rtpbin.request_pad_simple("recv_rtp_sink_0").unwrap())?;
        rtcp_src
            .static_pad("src")
            .unwrap()
            .link(&rtpbin.request_pad_simple("recv_rtcp_sink_0").unwrap())?;
        rtpbin
            .request_pad_simple("send_rtcp_src_0")
            .unwrap()
            .link(&rtcp_sink.static_pad("sink").unwrap())?;

        // Sinks are created when rtpbin exposes the stream (its SSRC is only known then).
        {
            let pipeline_w = pipeline.downgrade();
            let stats = stats.clone();
            let file = file.to_path_buf();
            let on_event = on_event.clone();
            let track_id_outer = track.track_id.clone();
            let arrivals_outer = arrivals.clone();
            rtpbin.connect_pad_added(move |_, pad| {
                let Some(pipeline) = pipeline_w.upgrade() else {
                    return;
                };
                if !pad.name().starts_with("recv_rtp_src_") {
                    return;
                }
                let Ok(depay) = gst::ElementFactory::make(depay_name)
                    .property_if("wait-for-keyframe", true, is_video)
                    .property_if("request-keyframe", true, is_video)
                    .build()
                else {
                    on_event(PipelineEvent::Error(format!("cannot create {depay_name}")));
                    return;
                };
                if is_video {
                    // On a gap the depayloader asks upstream for a keyframe, which rtpsession turns into PLI/FIR.
                    depay.set_property("request-keyframe", true);
                    depay.set_property("wait-for-keyframe", true);
                }
                let mux = gst::ElementFactory::make("matroskamux")
                    .build()
                    .expect("matroskamux");
                mux.set_property("streamable", true); // clusters written as we go: a crash keeps what came before
                let sink = gst::ElementFactory::make("filesink")
                    .build()
                    .expect("filesink");
                sink.set_property("location", file.to_string_lossy().to_string());
                // No userspace write buffer: a recorder killed mid-capture leaves everything it received on disk
                // (a mic track is ~3 KB/s, so a buffered sink would hold many seconds of audio in memory).
                sink.set_property_from_str("buffer-mode", "unbuffered");
                if pipeline.add_many([&depay, &mux, &sink]).is_err()
                    || gst::Element::link_many([&depay, &mux, &sink]).is_err()
                {
                    on_event(PipelineEvent::Error("cannot link depay/mux/sink".into()));
                    return;
                }
                for e in [&depay, &mux, &sink] {
                    let _ = e.sync_state_with_parent();
                }
                if pad.link(&depay.static_pad("sink").unwrap()).is_err() {
                    on_event(PipelineEvent::Error("cannot link rtpbin to depay".into()));
                    return;
                }
                {
                    let stats = stats.clone();
                    let trace = trace_rtp();
                    let track_id = track_id_outer.clone();
                    depay.static_pad("src").unwrap().add_probe(
                        gst::PadProbeType::BUFFER,
                        move |_, info| {
                            if trace {
                                if let Some(b) = info.buffer() {
                                    tracing::info!(target: "rtp_trace", track = %track_id, t = started.elapsed().as_millis() as u64,
                                        pts_ms = b.pts().map(|p| p.mseconds()).unwrap_or(0),
                                        key = !b.flags().contains(gst::BufferFlags::DELTA_UNIT), "frame out");
                                }
                            }
                            let mut s = stats.lock().unwrap();
                            s.frames_written += 1;
                            if s.first_frame_t.is_none() {
                                s.first_frame_t = Some(s.end_t);
                            }
                            gst::PadProbeReturn::Ok
                        },
                    );
                }

                // The file timeline is the RTP timestamp (see module doc), plus per-packet stats.
                let stats = stats.clone();
                // last rtp ts, unwrapped ticks since the first, last seq, arrival t (ms) of the last packet
                let state: PtsState = Arc::new(Mutex::new(None));
                let track_id_for_log = track_id_outer.clone();
                let trace = trace_rtp();
                let vp8 = depay_name == "rtpvp8depay";
                let arrivals = arrivals_outer.clone();
                let on_event = on_event.clone();
                let last_orientation: Arc<Mutex<Option<(u16, bool)>>> = Arc::new(Mutex::new(None));
                depay.static_pad("sink").unwrap().add_probe(
                    gst::PadProbeType::BUFFER,
                    move |_, info| {
                        let Some(buf) = info.buffer_mut() else {
                            return gst::PadProbeReturn::Ok;
                        };
                        if trace {
                            if let Ok(map) = buf.map_readable() {
                                trace_packet("jitterbuffer", &track_id_for_log, started, map.as_slice(), vp8);
                            }
                        }
                        let (seq, ts, len, cvo) = {
                            let Ok(map) = buf.map_readable() else {
                                return gst::PadProbeReturn::Ok;
                            };
                            let d = map.as_slice();
                            if d.len() < 12 {
                                return gst::PadProbeReturn::Ok;
                            }
                            (
                                u16::from_be_bytes([d[2], d[3]]),
                                u32::from_be_bytes([d[4], d[5], d[6], d[7]]),
                                d.len() as u64,
                                cvo_id.and_then(|id| rtp_extension_value(d, id)),
                            )
                        };
                        let now_t = match arrivals.lock().unwrap()[seq as usize] {
                            0 => started.elapsed().as_millis() as u64,
                            a => (a - 1) as u64,
                        };
                        if let Some(v) = cvo {
                            // 3GPP TS 26.114: R1R0 = rotation in steps of 90 degrees, F = horizontal flip.
                            let orientation = ((v & 0x03) as u16 * 90, v & 0x04 != 0);
                            let mut last = last_orientation.lock().unwrap();
                            if *last != Some(orientation) {
                                *last = Some(orientation);
                                on_event(PipelineEvent::Orientation {
                                    t: now_t,
                                    rotation: orientation.0,
                                    flip: orientation.1,
                                });
                            }
                        }
                        let mut st = state.lock().unwrap();
                        let unwrapped = match *st {
                            None => {
                                let mut s = stats.lock().unwrap();
                                s.first_rtp_ts = Some(ts);
                                s.start_t = Some(now_t);
                                0i64
                            }
                            Some((last_rtp, last, _, last_t)) => {
                                let mut delta = ts as i64 - last_rtp as i64;
                                if delta > i32::MAX as i64 {
                                    delta -= 1i64 << 32;
                                } else if delta < i32::MIN as i64 {
                                    delta += 1i64 << 32;
                                }
                                // The sender's clock can jump (a browser re-creating its encoder, a layer switch):
                                // a delta far from the arrival gap is not time passing, so the file timeline
                                // follows the arrival gap instead and stays continuous.
                                let expected = (now_t.saturating_sub(last_t) as i64) * clock_rate as i64 / 1000;
                                if (delta - expected).abs() > 2 * clock_rate as i64 {
                                    tracing::warn!(track = %track_id_for_log, rtp_delta = delta, expected,
                                        "rtp timestamp jump; timeline re-anchored on arrival time");
                                    delta = expected;
                                }
                                last + delta
                            }
                        };
                        *st = Some((ts, unwrapped, seq, now_t));
                        let pts_ns =
                            (unwrapped.max(0) as u128 * 1_000_000_000u128 / clock_rate) as u64;
                        let buf = buf.make_mut();
                        buf.set_pts(gst::ClockTime::from_nseconds(pts_ns));
                        buf.set_dts(gst::ClockTime::NONE);
                        let mut s = stats.lock().unwrap();
                        s.packets += 1;
                        s.bytes += len;
                        s.end_t = now_t;
                        gst::PadProbeReturn::Ok
                    },
                );
            });
        }

        // Loss the jitter buffer gave up on: rtpbin sends GstRTPPacketLost events downstream; count them as gaps.
        {
            let stats = stats.clone();
            let on_event = on_event.clone();
            let stopping = stopping.clone();
            let quiet = quiet.clone();
            let jitterbuffers = jitterbuffers.clone();
            rtpbin.connect("new-jitterbuffer", false, move |args| {
                let jb: gst::Element = args[1].get().ok()?;
                jitterbuffers.lock().unwrap().push(jb.clone());
                let stats = stats.clone();
                let on_event = on_event.clone();
                let stopping = stopping.clone();
                let quiet = quiet.clone();
                if let Some(src) = jb.static_pad("src") {
                    src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                        if let Some(gst::PadProbeData::Event(ev)) = &info.data {
                            if let gst::EventView::CustomDownstream(cd) = ev.view() {
                                if let Some(s) = cd.structure() {
                                    if s.name() == "GstRTPPacketLost"
                                        && !stopping.load(Ordering::Relaxed)
                                        && !quiet.load(Ordering::Relaxed)
                                    {
                                        let mut st = stats.lock().unwrap();
                                        let from_t = st.end_t;
                                        let now_t = (started.elapsed().as_millis() as u64)
                                            .saturating_sub(jitter_buffer_ms as u64)
                                            .max(from_t);
                                        if trace_rtp() {
                                            tracing::info!(target: "rtp_trace", t = now_t,
                                                seq = s.get::<u32>("seqnum").unwrap_or(0),
                                                n = s.get::<u32>("num-packets").unwrap_or(1), "packet lost");
                                        }
                                        let lost: u64 = s
                                            .get::<u32>("num-packets")
                                            .map(|n| n as u64)
                                            .unwrap_or(1);
                                        let gap = Gap {
                                            from_t,
                                            to_t: now_t,
                                            lost_packets: lost,
                                        };
                                        st.packets_lost += lost;
                                        st.gaps.push(gap);
                                        drop(st);
                                        on_event(PipelineEvent::Gap {
                                            from_t: gap.from_t,
                                            to_t: gap.to_t,
                                            lost_packets: lost,
                                        });
                                    }
                                }
                            }
                        }
                        gst::PadProbeReturn::Ok
                    });
                }
                None
            });
        }

        // Bus: errors end the track; EOS is awaited in `stop`.
        {
            let on_event = on_event.clone();
            let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
            bus.set_sync_handler(move |_, msg| {
                if let gst::MessageView::Error(e) = msg.view() {
                    on_event(PipelineEvent::Error(format!(
                        "{} ({:?})",
                        e.error(),
                        e.debug()
                    )));
                }
                gst::BusSyncReply::Pass
            });
        }

        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            pipeline,
            stats,
            rtcp_sink,
            stopping,
            quiet,
            jitterbuffers,
        })
    }

    /// Where the SFU listens for our RTCP (`capture.trackConnected`).
    pub fn connect_rtcp(&self, ip: &str, port: u16) {
        self.rtcp_sink.set_property("host", ip);
        self.rtcp_sink.set_property("port", port as i32);
    }

    /// Frames written to the file so far.
    /// (frames written to the file, key frames seen at the socket): either moving means the stream is decodable.
    pub fn progress(&self) -> (u64, u64) {
        let s = self.stats.lock().unwrap();
        (s.frames_written, s.keyframes_in)
    }

    /// A paused producer sends nothing: while `on`, the jitter buffer's timeouts are not losses.
    pub fn set_quiet(&self, on: bool) {
        self.quiet.store(on, Ordering::Relaxed);
    }

    /// Loss reported from here on is the stream ending, not the network; call before the sender goes away.
    pub fn mark_stopping(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        // No more retransmission retries from here: flushing a full jitter buffer at EOS with the retry timers
        // still armed runs away in GStreamer 1.26 (seconds of CPU and about 10 MB per buffered packet, per track;
        // an 8 GB host without swap went down on it). Turning `do-retransmission` off is not enough, the retry limits
        // must be zero. Set on the jitter buffers themselves: rtpbin does not pass the change on.
        for jb in self.jitterbuffers.lock().unwrap().iter() {
            jb.set_property("do-retransmission", false);
            jb.set_property("rtx-max-retries", 0i32);
            jb.set_property("rtx-deadline", 0i32);
            jb.set_property("rtx-retry-timeout", 0i32);
        }
    }

    /// EOS so the muxer writes its last cluster, then tear down. Blocks up to `timeout`.
    pub fn stop(&self, timeout: std::time::Duration) {
        self.mark_stopping();
        self.pipeline.send_event(gst::event::Eos::new());
        if let Some(bus) = self.pipeline.bus() {
            let _ = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(timeout.as_millis() as u64),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// SR blocks (PT 200) in a compound RTCP packet: (ntp seconds as f64, rtp timestamp).
/// `RECORDER_TRACE_RTP=1`: log every packet at the socket and after the jitter buffer, and every frame the
/// depayloader emits (target `rtp_trace`). Diagnostics only: hundreds of lines a second per video track.
fn trace_rtp() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RECORDER_TRACE_RTP").is_ok_and(|v| v == "1" || v == "true"))
}

/// The one-byte value of RTP header extension `id` (RFC 8285, one- and two-byte forms), if the packet carries it.
pub(crate) fn rtp_extension_value(d: &[u8], id: u8) -> Option<u8> {
    if d[0] & 0x10 == 0 {
        return None;
    }
    let off = 12 + 4 * (d[0] & 0x0F) as usize;
    if d.len() < off + 4 {
        return None;
    }
    let profile = u16::from_be_bytes([d[off], d[off + 1]]);
    let len = 4 * u16::from_be_bytes([d[off + 2], d[off + 3]]) as usize;
    let ext = d.get(off + 4..off + 4 + len)?;
    let mut i = 0;
    if profile == 0xBEDE {
        while i < ext.len() {
            let b = ext[i];
            if b == 0 {
                i += 1;
                continue;
            }
            let (eid, elen) = (b >> 4, (b & 0x0F) as usize + 1);
            if eid == 15 {
                break;
            }
            if eid == id {
                return ext.get(i + 1).copied();
            }
            i += 1 + elen;
        }
    } else if profile & 0xFFF0 == 0x1000 {
        while i + 1 < ext.len() {
            let (eid, elen) = (ext[i], ext[i + 1] as usize);
            if eid == 0 {
                i += 1;
                continue;
            }
            if eid == id {
                return ext.get(i + 2).copied();
            }
            i += 2 + elen;
        }
    }
    None
}

/// Where the payload starts in an RTP packet of at least 12 bytes (CSRCs and the header extension skipped);
/// clamped to the packet length.
fn rtp_payload_offset(d: &[u8]) -> usize {
    let mut off = 12 + 4 * (d[0] & 0x0F) as usize;
    if d[0] & 0x10 != 0 && d.len() >= off + 4 {
        off += 4 + 4 * u16::from_be_bytes([d[off + 2], d[off + 3]]) as usize;
    }
    off.min(d.len())
}

/// VP8 payload (RFC 7741): (starts a frame, starts a key frame). The descriptor's S bit and PID say whether a frame
/// starts here; the payload header's P bit after the descriptor is 0 for a key frame.
fn vp8_frame_bits(p: &[u8]) -> (bool, bool) {
    if p.is_empty() {
        return (false, false);
    }
    let start = p[0] & 0x10 != 0 && p[0] & 0x07 == 0;
    let mut n = 1;
    if p[0] & 0x80 != 0 && p.len() > 1 {
        let x = p[1];
        n = 2;
        if x & 0x80 != 0 {
            n += if p.get(n).is_some_and(|b| b & 0x80 != 0) {
                2
            } else {
                1
            };
        }
        if x & 0x40 != 0 {
            n += 1;
        }
        if x & 0x30 != 0 {
            n += 1;
        }
    }
    (start, start && p.get(n).is_some_and(|b| b & 0x01 == 0))
}

fn trace_packet(stage: &str, track_id: &str, started: Instant, d: &[u8], vp8: bool) {
    if d.len() < 12 {
        return;
    }
    let seq = u16::from_be_bytes([d[2], d[3]]);
    let ts = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
    let marker = d[1] & 0x80 != 0;
    let (start, key) = if vp8 {
        vp8_frame_bits(&d[rtp_payload_offset(d)..])
    } else {
        (false, false)
    };
    tracing::info!(target: "rtp_trace", track = %track_id, t = started.elapsed().as_millis() as u64, stage, seq, ts, marker, start, key, len = d.len());
}

fn parse_sender_reports(d: &[u8]) -> Vec<(f64, u32)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= d.len() {
        let pt = d[i + 1];
        let len = ((u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize) + 1) * 4;
        if len == 0 {
            break;
        }
        if pt == 200 && i + 20 <= d.len() {
            let ntp_sec = u32::from_be_bytes([d[i + 8], d[i + 9], d[i + 10], d[i + 11]]) as f64;
            let ntp_frac = u32::from_be_bytes([d[i + 12], d[i + 13], d[i + 14], d[i + 15]]) as f64;
            let rtp_ts = u32::from_be_bytes([d[i + 16], d[i + 17], d[i + 18], d[i + 19]]);
            out.push((ntp_sec + ntp_frac / 4_294_967_296.0, rtp_ts));
        }
        i += len;
    }
    out
}
