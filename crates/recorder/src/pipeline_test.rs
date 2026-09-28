//! `pipeline`: a synthetic RTP sender on the loopback becomes a playable WebM
//! with the statistics the manifest needs. Runs wherever GStreamer is installed (the build image, CI).

use crate::pipeline::{PipelineEvent, TrackPipeline, TrackPipelineOptions};
use crate::wire::{AllocateTrack, RtpCodec, RtpEncoding, RtpParameters, TrackKind};
use gst::prelude::*;
use gstreamer as gst;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The pipeline tests bind real ports: one at a time, or two tests pick the same free pair.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Two free consecutive UDP ports on the loopback (RTP even, RTCP odd), as the allocator hands out.
fn free_port_pair() -> (u16, u16) {
    for port in (47000..48000).step_by(2) {
        let rtp = UdpSocket::bind(("127.0.0.1", port));
        let rtcp = UdpSocket::bind(("127.0.0.1", port + 1));
        if rtp.is_ok() && rtcp.is_ok() {
            return (port, port + 1);
        }
    }
    panic!("no free port pair");
}

fn vp8_track(track_id: &str) -> AllocateTrack {
    AllocateTrack {
        recording_id: "r-test".into(),
        track_id: track_id.into(),
        peer_id: "p1".into(),
        kind: TrackKind::Webcam,
        codec: "video/VP8".into(),
        clock_rate: 90000,
        rtp_parameters: RtpParameters {
            codecs: vec![RtpCodec {
                mime_type: "video/VP8".into(),
                payload_type: 101,
                clock_rate: 90000,
                channels: None,
                parameters: Default::default(),
            }],
            encodings: vec![RtpEncoding {
                ssrc: Some(0x1234_5678),
                rtx: None,
            }],
            header_extensions: vec![],
        },
    }
}

/// `videotestsrc → vp8enc → rtpvp8pay → udpsink`, `frames` frames at 30 fps, run to EOS.
fn send_vp8(port: u16, frames: i32) {
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc num-buffers={frames} pattern=ball ! video/x-raw,width=320,height=180,framerate=30/1 \
         ! vp8enc deadline=1 keyframe-max-dist=15 ! rtpvp8pay pt=101 ssrc=305419896 mtu=1200 \
         ! udpsink host=127.0.0.1 port={port} sync=true"
    ))
    .expect("sender pipeline");
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let _ = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
}

/// Decoded frames in a WebM file, through the same demux/decode chain compose uses.
fn decoded_frames(path: &std::path::Path) -> u64 {
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! matroskademux ! vp8dec ! fakesink name=sink",
        path.display()
    ))
    .expect("probe pipeline");
    let sink = pipeline
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .by_name("sink")
        .unwrap();
    let frames = Arc::new(Mutex::new(0u64));
    {
        let frames = frames.clone();
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                *frames.lock().unwrap() += 1;
                gst::PadProbeReturn::Ok
            });
    }
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    if let Some(gst::MessageView::Error(e)) = msg.as_ref().map(|m| m.view()) {
        panic!("probe error: {}", e.error());
    }
    let n = *frames.lock().unwrap();
    n
}

#[test]
fn synthetic_vp8_stream_becomes_a_playable_webm_with_stats() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    gst::init().unwrap();
    let dir = std::env::temp_dir().join(format!("recorder-pipeline-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t1.webm");
    let (rtp_port, rtcp_port) = free_port_pair();
    let track = vp8_track("t1");
    let events: Arc<Mutex<Vec<PipelineEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    let pipeline = TrackPipeline::start(TrackPipelineOptions {
        track: &track,
        rtp_port,
        rtcp_port,
        bind_ip: "127.0.0.1",
        file: &file,
        jitter_buffer_ms: 200,
        started: Instant::now(),
        on_event: Box::new(move |e| sink_events.lock().unwrap().push(e)),
    })
    .expect("track pipeline");
    // Nobody listens there; the pipeline still has a valid RTCP target.
    pipeline.connect_rtcp("127.0.0.1", rtcp_port);

    send_vp8(rtp_port, 60);
    // The sender is gone; as the controller does on release, say so before the jitter buffer times out, or the
    // packet it still waits for would count as lost.
    pipeline.mark_stopping();
    std::thread::sleep(Duration::from_millis(600));
    pipeline.stop(Duration::from_secs(5));

    let stats = pipeline.stats.lock().unwrap();
    assert!(stats.packets >= 60, "packets received: {}", stats.packets);
    assert_eq!(
        stats.packets_lost, 0,
        "loss on the loopback: {:?}",
        stats.gaps
    );
    assert!(stats.first_rtp_ts.is_some(), "first RTP timestamp recorded");
    assert!(
        stats.start_t.is_some() && stats.end_t > stats.start_t.unwrap(),
        "start/end t"
    );
    assert!(stats.bytes > 0);
    drop(stats);

    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    assert!(size > 10_000, "webm written: {size} bytes");
    let frames = decoded_frames(&file);
    assert!(frames >= 55, "decoded frames: {frames}");
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PipelineEvent::Error(_))),
        "no pipeline errors"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn rss_mb() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    statm
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0)
        * 4096
        / (1024 * 1024)
}

/// Stopping a pipeline with a full jitter buffer must not allocate for every packet it flushes (an 8 GB host went down
/// on it: gigabytes at "capture stopping", per track).
#[test]
fn stopping_with_a_full_jitter_buffer_does_not_balloon() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    gst::init().unwrap();
    let dir = std::env::temp_dir().join(format!("recorder-pipeline-stop-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t2.webm");
    let (rtp_port, rtcp_port) = free_port_pair();
    let track = vp8_track("t2");
    let pipeline = TrackPipeline::start(TrackPipelineOptions {
        track: &track,
        rtp_port,
        rtcp_port,
        bind_ip: "127.0.0.1",
        file: &file,
        jitter_buffer_ms: 3000,
        started: Instant::now(),
        on_event: Box::new(|_| {}),
    })
    .expect("track pipeline");
    pipeline.connect_rtcp("127.0.0.1", rtcp_port);
    send_vp8(rtp_port, 150);
    let before = rss_mb();
    let t = Instant::now();
    pipeline.stop(Duration::from_secs(10));
    let after = rss_mb();
    eprintln!(
        "stop took {} ms, rss {} MB -> {} MB, packets {}",
        t.elapsed().as_millis(),
        before,
        after,
        pipeline.stats.lock().unwrap().packets
    );
    assert!(after < before + 200, "stop allocated {} MB", after - before);
}

#[test]
fn video_orientation_extension_is_read_in_both_header_forms() {
    use crate::pipeline::rtp_extension_value;
    // One-byte form (0xBEDE): id 13, length 1, value 0b0011 (rotation 270, no flip), then padding.
    let one = [
        0x90, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0xBE, 0xDE, 0, 1, 0xD0, 0x03, 0, 0,
    ];
    assert_eq!(rtp_extension_value(&one, 13), Some(0x03));
    assert_eq!(rtp_extension_value(&one, 4), None);
    // Two-byte form (0x1000): id 13, length 1, value 1 (rotation 90).
    let two = [
        0x90, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0x00, 0, 1, 13, 1, 0x01, 0,
    ];
    assert_eq!(rtp_extension_value(&two, 13), Some(0x01));
    // No extension bit: nothing.
    let none = [0x80, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(rtp_extension_value(&none, 13), None);
}
