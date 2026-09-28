# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
the project follows [Semantic Versioning](https://semver.org/). The control protocol, the spool format and the manifest
are the public contract: a breaking change to any of them is a major version.

## [1.1.1] - 2026-09-28

### Added

- The container image is published for linux/arm64 as well as linux/amd64, each built and tested on a machine of its own
  architecture. Apple Silicon and ARM servers run it natively.

## [1.1.0] - 2026-09-28

### Added

- Status events carry `durationMs`: the captured length with `captured`, the composite's length with `ready`. An
  application can show how long a recording is without reading the manifest.

## [1.0.0] - 2026-09-28

First public release.

### Capture

- One WebM file per track, written as received (VP8, VP9, H264, Opus), with no decoding during the meeting.
- RTP over plain transports with retransmission and keyframe requests, a keyframe chase after connect and after every
  unrecovered gap, and a jitter buffer sized to ride through mediasoup's layer-sync hole.
- An append-only event log and a manifest with per-track statistics and sender reports for alignment.
- Camera orientation read from `urn:3gpp:video-orientation`.
- Limits: maximum recording length, minimum free spool space, orphan grace after the controller is lost.
- Captures interrupted by a crash are finalised from disk on the next start.

### Compose

- Offline composition to H.264 and AAC in MP4, a thumbnail, and a mixed audio file, faster than real time.
- The `auto` layout policy as data: grid, shared screen with a filmstrip, slates, name labels with status icons, speaker
  border, animated transitions, and a title card.
- Any number of compose workers, on the capture host or elsewhere.
- `recorder compose-local <dir>` composes a folder with no other service.

### Integration

- A socket.io control protocol for the process that owns the SFU session.
- Optional Redis for discovery, status events and the compose queue.
- Optional upload to any S3-compatible store; without it the recording stays in the spool.
- A reference controller for mediasoup and an end-to-end example that needs no browser.

[1.1.1]: https://github.com/scriptechn/mediasoup-recorder/releases/tag/v1.1.1
[1.1.0]: https://github.com/scriptechn/mediasoup-recorder/releases/tag/v1.1.0
[1.0.0]: https://github.com/scriptechn/mediasoup-recorder/releases/tag/v1.0.0
