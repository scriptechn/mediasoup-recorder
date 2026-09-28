# Control protocol

The recorder is a socket.io server (protocol v5, Engine.IO 4; `socket.io-client` 4.x connects). The controller opens one
connection per recorder and multiplexes every recording over it.

## Handshake

Query parameters: `clientId` (any non-empty id of the controller; `roomServerId` is accepted as an older name) and
`secret`, compared with `RECORDER_SECRET`. A bad secret or a missing id is disconnected at once.

Once accepted the recorder emits the notification `recorderReady { load }`.

## Events

Two socket.io events each way:

- `notification`: fire and forget, payload `{ method, data }`.
- `request`: payload `{ method, data }`, answered by an ack of two arguments `(serverError, response)`. `serverError` is
  a string or `null`; `response` is an object. Every successful response carries `load`.

Unknown methods are answered with an error and logged. Unknown enum values are rejected at the boundary.

### Requests, controller → recorder

| method                   | data                                                                                                    | response                 |
| ------------------------ | ------------------------------------------------------------------------------------------------------- | ------------------------ |
| `capture.start`          | see below                                                                                               | `{ ok: true }` or error  |
| `capture.allocateTrack`  | `recordingId, trackId, peerId, kind: mic\|webcam\|screen, codec, clockRate, rtpParameters`              | `{ ip, port, rtcpPort }` |
| `capture.trackConnected` | `recordingId, trackId, consumerId?, rtcp: { ip, port }` (where the SFU listens for the recorder's RTCP) | `{ ok: true }`           |
| `capture.releaseTrack`   | `recordingId, trackId`                                                                                  | `{ ok: true }`           |
| `capture.event`          | `recordingId, event` (one `events.jsonl` line without `t`; `type` required, `at` stamped when missing)  | `{ ok: true }`           |
| `capture.stop`           | `recordingId, reason: StopReason`                                                                       | `{ ok: true }`           |

`capture.start` data:

| field           | required | meaning                                                                                        |
| --------------- | -------- | ---------------------------------------------------------------------------------------------- |
| `recordingId`   | yes      | Unique; also the spool folder name and the last segment of the default prefix                  |
| `startedAt`     | yes      | Epoch ms, for the manifest and the title card                                                  |
| `prefix`        | no       | Bucket prefix for this recording, no leading or trailing slash                                 |
| `accountId`     | no       | Application ids, copied into the manifest and the event log. When `prefix` is absent and both  |
| `scopeId`       | no       | `accountId` and `scopeId` are given, the prefix is `<accountId>/<scopeId>/<recordingId>`;      |
| `callId`        | no       | otherwise it is `<recordingId>`. `meetingId` is accepted as an older name for `scopeId`.       |
| `sessionId`     | no       |                                                                                                |
| `metadata`      | no       | Any JSON object, stored verbatim in the manifest                                               |
| `title`         | no       | Title card text; "Meeting recording" when absent                                               |
| `startedByName` | no       | Title card: who pressed record                                                                 |
| `policy`        | no       | `{ name, version }`; default `{ auto, 1 }`. The name selects `policies/<name>.json` at compose |
| `peers`         | no       | `PeerInfo[]` of everyone already in the room; each becomes a `peer.joined` line                |

`PeerInfo`: `{ peerId, userId?, name?, picture?, userType? }`. Only `peerId` matters to the recorder; `name` is drawn on
labels and slates, `picture` is stored for a future layout, `userType` is free-form.

`rtpParameters` in `capture.allocateTrack` is the consumer's, as the SFU created it:
`codecs[] { mimeType, payloadType, clockRate, channels?, parameters }`, `encodings[] { ssrc?, rtx? }`,
`headerExtensions[] { uri, id }`. Payload types and SSRCs are the SFU's choice and the recorder configures its caps from
them. The one header extension it reads is `urn:3gpp:video-orientation`.

Errors from `capture.start`: `disk_full` (below `RECORDER_MIN_FREE_BYTES`), `recording <id> already running here`.

### Notifications, recorder → controller

| method          | data                                  | what the controller does                              |
| --------------- | ------------------------------------- | ----------------------------------------------------- |
| `recorderReady` | `{ load }`                            | Marks the connection usable                           |
| `needKeyFrame`  | `{ recordingId, trackId }`            | `consumer.requestKeyFrame()` on that track's consumer |
| `captureEnded`  | `{ recordingId, reason: StopReason }` | The recorder ended it (cap, disk, orphaned): clean up |

`load` is 0..1, the fraction of the recorder's UDP port pairs in use. A controller with several recorders picks the
lowest.

### Disconnect

When the controller's socket drops, every capture it owned enters the orphan grace (`RECORDER_ORPHAN_GRACE_MS`); with no
reconnect that claims it, the recorder stops it with reason `room_server_lost`.

## Vocabulary

```
RecordingStatus = starting | recording | stopping | captured | composing | ready | failed
StopReason      = user | room_closed | media_node_lost | recorder_lost | room_server_lost | disk_full
                | max_duration | retention
FailReason      = capture_no_media | upload_failed | compose_failed | spool_missing | cancelled
TrackKind       = mic | webcam | screen
```

Event types are listed in `spool-format.md`.

## Redis (optional)

Configured by `REDIS_HOST`. Channel names are scoped by database number: `<db>:<name>`.

**Discovery.** Hash `recorders:`, field `<id>` → `{ id, host, port, tls, load }`, written at start, every 10 s with the
current load, and deleted at shutdown. Channel `recorders` carries `{ type: RECORDER_ADDED, message: entry }`,
`{ type: RECORDER_LOAD, message: { recorderId, load } }` and `{ type: RECORDER_REMOVED, message: id }`.

**Status.** Channel `recording-events`, one JSON object per message:

```json
{
  "recordingId": "…",
  "status": "captured",
  "at": 1789420000000,
  "stopReason": "user",
  "failReason": null,
  "failDetail": null,
  "artifacts": ["tracks/p1/c-9f2.webm", "events.jsonl", "manifest.json"]
}
```

`stopReason` comes with `captured` and `failed` after a capture; `failReason` and `failDetail` with `failed`;
`artifacts` (keys relative to the prefix) with `captured` and `ready`. Treat it as a hint: the manifest in the bucket is
the truth for what exists.

**Compose queue.** List `recording-compose`; items are bucket prefixes. The capture role pushes after a successful
upload; an operator pushes to re-run. Claims are `recording-compose-claim:<recordingId>` with an expiry.
