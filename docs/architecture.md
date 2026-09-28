# Architecture

## The rule

**The recording is the event log plus the per-track files. Everything a user watches is derived from those two, and can
be derived again.**

Three consequences carry the design:

1. **Capture never composes.** During the meeting the recorder only writes what the SFU sends it, one file per track,
   plus one line per room event. Near-zero CPU, so it can sit beside the SFU without hurting live calls.
2. **Compose is a job, not a session.** After the meeting a separate job reads the tracks and the log and renders the
   video, faster than real time, at low priority, on any machine. A bad layout is fixed by re-running the job on old
   recordings; nothing is lost.
3. **A partial recording is still a recording.** Files are written so that a crash at any second leaves everything
   before it playable. The compose job renders whatever exists and says what is missing.

## Components

```
application ⇄ controller (owns the SFU session) ⇄ SFU (mediasoup)
                     │ socket.io, secret               │ RTP/RTCP over PlainTransport
                     ▼                                 ▼
                 recorder (capture) ◀──────────────────┘
application ◀── Redis pub/sub "recording-events" ── recorder (capture + compose)     [optional]
application ──▶ S3 (list, serve, delete)             recorder ──▶ S3 (upload)        [optional]
```

- **recorder**: one binary, two roles selected by `RECORDER_ROLE` (`capture`, `compose`, `both`; default `both`).
  - **capture** is a socket.io server the controller connects to (`docs/protocol.md`). It holds one GStreamer pipeline
    per track, writes per-track files and `events.jsonl` to a local spool, and at stop writes `manifest.json`, uploads
    the folder when a bucket is configured, and publishes status when Redis is configured.
  - **compose** is a worker in the same binary. The truth for "what needs composing" is the manifest: every
    `manifest.json` without a `composite` entry is a pending job. The worker scans its own spool at start and every 30 s
    while idle. With Redis, the list `recording-compose` is a wake-up so it does not have to wait for the scan; an item
    it does not hold locally is fetched from the bucket first, which is how a compose-only host works. It renders
    `composite.mp4`, `thumbnail.jpg` and `audio.m4a`, rewrites `manifest.json`, uploads those four when there is a
    bucket, publishes `composing` then `ready` (or `failed compose_failed`), and only then deletes the spool. Without a
    bucket the composite stays next to its tracks. It runs on a thread at `nice 15`, one recording at a time per worker.
    Any number of workers may run: before a job the worker reads the bucket's manifest, and a composite there means
    another worker finished it; then it claims the recording in Redis (`recording-compose-claim:<recordingId>`, SET NX
    with an expiry of the maximum recording length). A compose-only worker never waits for captures; on a capture host
    the worker waits for live captures to end unless `RECORDER_COMPOSE_WHILE_CAPTURING=true`, because a busy encoder
    makes the capture threads drop packets.
- **controller**: owns the live capture session: which SFU router, which consumers, which transports, and the forwarding
  of room events to the recorder. It does not know about storage or compose. `examples/mediasoup/` is a complete one.
- **application**: whatever faces users. It authorises start and stop, keeps its own record of each recording, listens
  to `recording-events` (or reads the manifest) to learn when the composite is ready, and serves the files from the
  bucket. It never touches media, spool or the compose queue.

Two kinds of event, two senders: **room events** (join, leave, mute, share, speaker, chat) go from the controller to the
recorder over the control socket and become `events.jsonl` lines; **status events** (`RecordingStatus` changes) go from
the recorder to the application over Redis. Status goes over pub/sub rather than back over the control socket because
the recorder outlives the session that started it.

## Why plain RTP and not a WebRTC peer

mediasoup is a router, not an endpoint, and ships no recorder. Its documented egress is `PlainTransport` into FFmpeg or
GStreamer. The worker routes NACK, PLI and FIR from any transport to the consumer, so a plain-transport consumer
recovers loss if the endpoint asks; the well-known "no retransmission on plain transport" complaint is about `ffmpeg`
and GStreamer's `sdpdemux` never asking. A hand-built `rtpbin` with `do-retransmission=true` does ask. Between an SFU
and a recorder on the same host or private network, loss is near zero anyway; loss on the participant's link is already
recovered by the SFU before the packet reaches us.

The alternative, a full WebRTC peer, needs an SDP translator mediasoup does not provide, ICE and DTLS to manage, and
gains nothing on a private network.

## Transport details

- One plain transport per consumer, `comedia: false`, `rtcpMux: false`. The recorder allocates the RTP and RTCP ports
  from its own UDP range (`RECORDER_RTC_MIN_PORT`..`RECORDER_RTC_MAX_PORT`, default 41000-41999) and returns them; the
  controller connects the transport to them. The recorder container runs with host networking so the range is reachable.
- The consumer's `rtpCapabilities` are the recorder's real capabilities: VP8, VP9, H264 and Opus with `nack`, `nack pli`
  and `ccm fir` declared, no `transport-cc` or `goog-remb`, and no RTX (`docs/mediasoup.md` explains why).
- Video consumers pin the top layer (`setPreferredLayers({ spatialLayer: 2, temporalLayer: 2 })`).
- Keyframes: the controller asks for one right after creating the consumer; the recorder asks over the control socket
  (`needKeyFrame { recordingId, trackId }`) every second, from connect and again after every gap it could not recover,
  until frames reach the file (up to 30 s). One request is not enough: a browser has been seen to answer only the third.
  The depayloaders wait for a keyframe after a loss, so the file only ever holds decodable frames.
- Timeline: each track file's timeline is its RTP timestamp, rewritten to `(rtp - firstRtp) / clockRate` after the
  jitter buffer; rtpbin's own clock-skew estimate never reaches the file. Alignment across tracks comes from the Sender
  Report mapping plus `firstRtpTs` in the manifest. `t` (the event clock) is taken at the socket, not after the jitter
  buffer, so it does not move with the buffer's size.
- The jitter buffer is 3 s by default (`RECORDER_JITTER_BUFFER_MS`); packets are never declared lost inside it. This
  rides through mediasoup's layer-sync hole on freshly created simulcast consumers (`docs/mediasoup.md`).
- At stop, retransmission retries are zeroed on every jitter buffer before EOS: GStreamer 1.26 otherwise flushes a full
  buffer with the retry timers armed, at seconds of CPU and about 10 MB per buffered packet per track. A unit test
  measures the process at stop.
- A camera's orientation (`urn:3gpp:video-orientation`, sent by phones) is read per packet and logged as
  `recorder.orientation`; compose turns the picture accordingly.

## Flows and edge cases

"C" is the controller, "R" the recorder's capture role, "J" the compose job.

**Start.** C picks a recorder (from the Redis registry by lowest load, or by configuration), connects, sends
`capture.start`. R refuses when free spool space is below `RECORDER_MIN_FREE_BYTES` (error `disk_full`) or when the id
is already live. Otherwise it creates the spool, writes `recording.started` and one `peer.joined` per peer given, arms
the length cap and answers `{ ok }`. C then forwards every current producer (next flow).

**A track appears** (join with media, camera on, share start). C sends `capture.allocateTrack` with the consumer's
`rtpParameters`; R reserves a UDP port pair, starts the pipeline and answers `{ ip, port, rtcpPort }`. C connects the
plain transport there, resumes the consumer, asks for a keyframe, and sends `capture.trackConnected` with where the SFU
listens for RTCP. R writes `track.started` and starts the keyframe chase for video. A track that never delivers a packet
is simply absent data (a paused camera produces nothing and that is not an error).

**Pause and resume** (mute, camera off, hold). C forwards `track.paused` / `track.resumed`; the consumer stays and the
file keeps its timeline (silence or no frames is absent data compose fills). `peer.hold` / `peer.unhold` make compose
treat the peer as camera off and muted.

**A track ends.** C closes the consumer and transport and sends `capture.releaseTrack`; R finalises the file and
remembers its statistics for the manifest. `track.stopped` and `peer.left` are C's event lines.

**Rejoin, second device, same user twice.** Every join is a new `peerId` and a new `peer.joined` line; compose groups
tiles by `peerId`, not `userId`, so two devices are two tiles, and a rejoin is a new tile. No merging, no guessing.

**Speaker, hand, chat.** `speaker.changed`, `hand.raised` / `hand.lowered`, `chat.message`. Chat is logged so a later
layout can render it; the `auto` policy does not draw it.

**Stop.** C sends `capture.stop { reason }`. R marks every pipeline stopping, sends EOS to all of them at once, waits
for in-flight releases, writes `recording.stopped` and the manifest, answers, and then uploads on its own. `captured`
goes out only when the bucket holds the manifest (or, without a bucket, at once with the local file list), followed by
the compose wake-up.

**Room closes while recording.** The same stop path with reason `room_closed`. The capture is not a peer of the room and
never keeps it open.

**Recorder dies mid-meeting.** C loses the socket and ends the session with reason `recorder_lost`. The spool keeps what
was captured (the file sink is unbuffered). On the next start, before the worker runs, every `<recordingId>/` with an
event log but no manifest is finalised from disk (`orphans.rs`): a manifest with `stopReason: recorder_lost`, the track
list from `track.allocated` lines, the end time from the newest track file. It is then uploaded and announced
`captured`, and composed like any other.

**Controller dies mid-meeting.** R loses the control socket. After `RECORDER_ORPHAN_GRACE_MS` (default 30 s) without a
reconnect, the capture is stopped with reason `room_server_lost` and proceeds as a normal stop. There is no re-claim of
a live capture by a new controller connection in this version.

**SFU dies mid-meeting.** C ends the session with reason `media_node_lost`. The recording up to that point is complete.

**Application dies.** Nothing in the media path depends on it. Status events it missed are recoverable from the
manifests in the bucket, which are the truth for `ready`.

**Two recordings in one room.** Sequential is normal; each is its own id and its own prefix. Concurrent recordings of
one room are the controller's decision; the recorder does not care.

**Codecs and layers.** VP8, VP9 and H264 are written as received; compose decodes all three. With simulcast the recorder
pins the top layer; if the producer stops sending it the SFU picks a lower one and the file's resolution changes
mid-stream, which WebM allows and compose scales per frame.

**Long meetings, disk, upload.** A recording never runs longer than `RECORDER_MAX_RECORDING_MS` (default two hours);
reaching it is a normal stop with reason `max_duration`, announced to the controller as `captureEnded`. Spool need per
hour at typical rates: about 0.5 GB per camera, 1 GB per screen share, 30 MB per mic. `capture.start` is refused below
`RECORDER_MIN_FREE_BYTES` (default 5 GiB). An upload failure after the S3 client's own retries is
`failed upload_failed`; the spool is kept, and an operator re-queues it by pushing the prefix to `recording-compose`
(the worker downloads nothing it already has and uploads the missing files).

**Compose failure.** J publishes `failed compose_failed` with the GStreamer error text in `failDetail`. Tracks and
events are already in the bucket, so nothing is lost; an operator re-queues with
`redis-cli rpush recording-compose <prefix>` after a fix, or runs `recorder compose-local` on the folder.

**Missing track file at compose.** The track is treated as camera off for its whole span and listed in
`composite.missing`.
