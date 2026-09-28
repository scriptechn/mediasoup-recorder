# mediasoup-recorder

Server-side meeting recording for SFU egress, built for [mediasoup](https://mediasoup.org). A single Rust binary with
two roles:

- **capture** runs during the meeting. The SFU forwards each participant's RTP to it over plain transports; it writes
  one WebM file per track and an append-only event log to a local spool, at near-zero CPU. Nothing is decoded or
  composed while people are talking.
- **compose** runs after the meeting. It reads the tracks and the event log, lays them out with a data-driven policy
  (grid, shared screen with a filmstrip, slates for cameras that are off, name labels, speaker border, a title card),
  and renders `composite.mp4`, `thumbnail.jpg` and a mixed `audio.m4a`, faster than real time, at low priority, on any
  machine.

The recording is the event log plus the per-track files. Everything a user watches is derived from those and can be
derived again: a better layout is a re-run of compose over old recordings, and a crash at any second leaves everything
before it playable.

## What you need

- An SFU that can forward a consumer's RTP/RTCP to a UDP address and forward keyframe requests back. mediasoup's
  `PlainTransport` does exactly this; `docs/mediasoup.md` and `examples/mediasoup/` show the integration.
- A **controller**: the process that owns the SFU session and tells the recorder what to record. It speaks a small
  socket.io protocol (`docs/protocol.md`). In mediasoup deployments this is the Node process that holds the routers.
- Linux with GStreamer 1.26 or later at runtime (the Dockerfile builds a Debian image with everything in it). The
  published image runs on linux/amd64 and linux/arm64.

Optional:

- **S3** (AWS, MinIO, Garage, Ceph RGW, anything path-style): finished recordings are uploaded under a prefix and the
  spool is cleared. Without it the spool is the destination.
- **Redis**: recorder discovery for controllers, a status channel for the application, and a wake-up queue for compose
  workers on other hosts. Without it the recorder is addressed directly and logs its status.

## Quick start

```sh
# the released image, or build it yourself with: docker build -t recorder .
docker pull ghcr.io/scriptechn/mediasoup-recorder:1
docker tag ghcr.io/scriptechn/mediasoup-recorder:1 recorder

# Capture to a local folder, compose in the same process, no Redis, no bucket.
docker run --rm --network host \
  -e RECORDER_ANNOUNCED_IP=10.0.0.5 -e RECORDER_SECRET=change-me \
  -v "$PWD/spool:/spool" recorder
```

Then point a controller at `http://10.0.0.5:3100` with the secret and drive it (`examples/mediasoup/`). When the
recording stops, `spool/<recordingId>/` holds the tracks, `events.jsonl`, `manifest.json`, and a few seconds later the
composite.

To compose a folder by hand, for instance after changing the layout policy:

```sh
docker run --rm -v "$PWD/spool:/work" recorder recorder compose-local /work/<recordingId>
```

## Documentation

| Document                | What it covers                                                                     |
| ----------------------- | ---------------------------------------------------------------------------------- |
| `docs/architecture.md`  | The two roles, who talks to whom, and every failure mode with what happens in each |
| `docs/protocol.md`      | The control socket, discovery in Redis, the status channel and the compose queue   |
| `docs/spool-format.md`  | `events.jsonl`, `manifest.json`, the storage layout and the event vocabulary       |
| `docs/layout.md`        | The `auto` layout policy and how compose turns events into a scene                 |
| `docs/configuration.md` | Every environment variable                                                         |
| `docs/mediasoup.md`     | The mediasoup integration: capabilities, plain transports, keyframes, known traps  |

## Building and checking

Rust toolchain pinned by `rust-toolchain.toml`; GStreamer development libraries are needed to build (Linux). On any
machine with Docker:

```sh
docker build --target build -t recorder-check .
docker run --rm -v "$PWD:/work" -w /work -e CARGO_TARGET_DIR=/tmp/target recorder-check ./scripts/check.sh
```

`scripts/check.sh` is the one check list: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`. The tests cover
the scene evaluation, the slates and title card, the shipped policy, the manifest formats, orphan finalisation, and the
capture pipeline itself (a synthetic VP8 RTP sender on the loopback becomes a playable WebM with the manifest
statistics). `.github/workflows/check.yml` runs the same script.

## Layout of the repository

```
crates/recorder/src/
  main.rs        entry point, the `health` and `compose-local` commands
  config.rs      environment → Config
  control.rs     the socket.io control server
  wire.rs        request, response and status types
  capture.rs     one Recording per live capture: tracks, keyframe chase, stop, manifest
  pipeline.rs    the per-track GStreamer pipeline (RTP in, WebM out)
  spool.rs       spool folder, events.jsonl, manifest.json
  orphans.rs     finalising captures a crashed recorder left behind
  storage.rs     S3 upload and download
  registry.rs    Redis: discovery, status, compose queue
  worker.rs      the compose worker
  compose.rs     the offline compose pipeline
  scene.rs       event log → timeline of layouts (pure)
  raster.rs      slates, labels, title card (pure)
  policy.rs      layout policy file
policies/auto.json
examples/mediasoup/
docs/
```

## License

Apache-2.0, see `LICENSE`.

This project is not affiliated with or endorsed by the mediasoup project. It is an independent recorder that works with
mediasoup.

The container image carries third-party software under its own licences, installed from Debian: GStreamer and its plugin
sets (LGPL), x264 (GPL) for the H.264 encoder, and FFmpeg through `gstreamer1.0-libav`. H.264 and AAC may need patent
licences in some countries when you distribute or serve the composite. The recorder itself contains none of this code;
it drives GStreamer at run time.

## Authors

Written by Saif ([@saif-o99](https://github.com/saif-o99)) and Qusai Mo
([@qusaieilouti99](https://github.com/qusaieilouti99)) at Script Technical Solutions. See `AUTHORS`.
