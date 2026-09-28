# Spool and storage format

## The spool folder

```
<RECORDER_SPOOL_DIR>/<recordingId>/
  events.jsonl                 append-only, one JSON object per line, flushed per line
  tracks/<peerId>/<trackId>.webm   one file per track, the codec the participant sent, no re-encode
  manifest.json                written once at stop, rewritten by compose
  composite.mp4                written by compose
  thumbnail.jpg                written by compose
  audio.m4a                    written by compose (all mics mixed)
```

Ids in paths are sanitised to `[A-Za-z0-9_-]`. A folder with `events.jsonl` and no `manifest.json` is an interrupted
capture and is finalised on the next start (`architecture.md`).

## `events.jsonl`

Written by the capture role from what the controller forwards plus what the pipelines observe. `t` is milliseconds since
`recording.started` on the recorder's monotonic clock, taken at the moment the event was received; `at` is the sender's
wall clock in epoch ms, so clock skew is measurable, never assumed.

```json
{"t":0,"at":1789420000000,"type":"recording.started","recordingId":"…","prefix":"acc/grp/…","accountId":"acc","scopeId":"grp","layoutPolicy":"auto","policyVersion":1,"recorderId":"rec-1"}
{"t":12,"at":…,"type":"peer.joined","peerId":"p1","userId":"u1","name":"Saif","picture":"…"}
{"t":15,"at":…,"type":"track.allocated","trackId":"c-9f2","peerId":"p1","kind":"webcam","codec":"video/VP8","file":"tracks/p1/c-9f2.webm"}
{"t":40,"at":…,"type":"track.started","trackId":"c-9f2","consumerId":"…"}
{"t":8400,"at":…,"type":"speaker.changed","peerId":"p1"}
{"t":20100,"at":…,"type":"track.paused","trackId":"c-9f2"}
{"t":41000,"at":…,"type":"share.started","peerId":"p2","trackId":"c-a10"}
{"t":90210,"at":…,"type":"peer.left","peerId":"p1"}
{"t":95000,"at":…,"type":"recorder.gap","trackId":"c-a10","fromT":94200,"toT":95000,"lostPackets":31}
{"t":180000,"at":…,"type":"recording.stopped","reason":"user"}
```

Event types:

| written by | type                                                                                                     |
| ---------- | -------------------------------------------------------------------------------------------------------- |
| recorder   | `recording.started`, `recording.stopped`, `track.allocated`, `track.started`, `recorder.gap`,            |
|            | `recorder.keyframe_requested`, `recorder.orientation`, `recorder.track_error`                            |
| controller | `peer.joined`, `peer.left`, `peer.renamed`, `peer.hold`, `peer.unhold`, `track.paused`, `track.resumed`, |
|            | `track.stopped`, `speaker.changed`, `hand.raised`, `hand.lowered`, `share.started`, `share.stopped`,     |
|            | `chat.message`                                                                                           |

Fields the `auto` layout reads: `peerId` on peer, speaker, hand, hold and share events; `trackId` on track and share
events; `name` on `peer.joined` and `peer.renamed`; `rotation` and `flip` on `recorder.orientation`. Anything else is
passed through and kept.

## `manifest.json`

Written at stop by capture, rewritten by compose. The truth for what artifacts exist: a manifest in the bucket means
every file it names is there (tracks and the log are uploaded first, the manifest last).

```json
{
  "version": 1,
  "recordingId": "…",
  "prefix": "acc/grp/…",
  "accountId": "acc",
  "scopeId": "grp",
  "callId": "…",
  "sessionId": "…",
  "metadata": { "anything": "the application sent" },
  "recorderId": "rec-1",
  "startedAt": 1789420000000,
  "stoppedAt": 1789420180000,
  "stopReason": "user",
  "durationMs": 180000,
  "title": "Weekly sync",
  "startedByName": "Saif",
  "policy": { "name": "auto", "version": 1 },
  "peers": [{ "peerId": "p1", "userId": "u1", "name": "Saif", "picture": null }],
  "tracks": [
    {
      "trackId": "c-9f2",
      "peerId": "p1",
      "kind": "webcam",
      "codec": "video/VP8",
      "clockRate": 90000,
      "file": "tracks/p1/c-9f2.webm",
      "startT": 40,
      "endT": 90210,
      "bytes": 1234567,
      "firstRtpTs": 123456,
      "senderReports": [{ "t": 5000, "ntpS": 1789420005.1, "rtpTs": 573456 }],
      "packets": 8100,
      "packetsLost": 3,
      "gaps": [{ "fromT": 94200, "toT": 95000, "lostPackets": 31 }]
    }
  ],
  "composite": {
    "file": "composite.mp4",
    "codec": "h264/aac",
    "width": 1280,
    "height": 720,
    "fps": 30,
    "bytes": 45000000,
    "durationMs": 180000,
    "policy": "auto",
    "policyVersion": 1,
    "composedAt": 1789420300000,
    "missing": []
  },
  "thumbnail": { "file": "thumbnail.jpg", "atT": 0 },
  "audioMix": { "file": "audio.m4a" }
}
```

`accountId`, `scopeId`, `callId`, `sessionId`, `metadata`, `title` and `startedByName` are present only when the
controller sent them. `prefix` is always written; manifests from before it existed derive it from `accountId` and
`scopeId` (`meetingId` is read as `scopeId`).

**Alignment.** A track's `startT` is the `t` of its first written frame (for video the first keyframe, which can come
well after the first packet; the composite shows the slate until then). Its file timeline is
`(rtp - firstRtpTs) / clockRate`. Each `senderReports` entry maps an RTP timestamp to the sender's NTP time at the
moment the report arrived (`t`); compose places every track on the recording clock from these, so it never trusts either
clock alone. A track without usable reports falls back to its first packet's arrival.

## Storage layout

With a bucket, the spool folder is uploaded under its prefix with the same relative names:

```
<prefix>/manifest.json
<prefix>/events.jsonl
<prefix>/composite.mp4
<prefix>/thumbnail.jpg
<prefix>/audio.m4a
<prefix>/tracks/<peerId>/<trackId>.webm
```

The prefix is `capture.start`'s `prefix`, else `<accountId>/<scopeId>/<recordingId>`, else `<recordingId>`. Uploads are
multipart in 8 MiB parts, four in flight; an `AbortIncompleteMultipartUpload` lifecycle rule on the bucket covers a
recorder that died mid-upload. Retention is the application's business: delete the prefix.
