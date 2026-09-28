# mediasoup integration

The recorder was built against mediasoup 3.26 and tested with browsers (Chromium) and React Native clients producing VP8
simulcast and Opus. `examples/mediasoup/` is a complete controller; this page is what it does and why.

## Capabilities

Consume for the recorder with capabilities derived from the router's, not the browser's:

- Codecs: `video/VP8`, `video/VP9`, `video/H264`, `audio/opus`, each with `rtcpFeedback` set to `nack`, `nack pli`,
  `ccm fir` for video and `nack` for audio. No `transport-cc`, no `goog-remb`: there is no bandwidth estimation on a
  plain transport.
- **No RTX codec.** Without one mediasoup answers a NACK with the original packet (`RtpStreamSend::ReceiveNack`), which
  the recorder's jitter buffer takes as a late arrival. With RTX negotiated the resend is RTX-wrapped and the recorder,
  which has no RTX receiver, drops it.
- Header extensions: only `urn:3gpp:video-orientation`. Phones send the camera in sensor orientation and say in that
  extension how far to turn it; the recorder logs every change and compose rotates the picture.
- Consume with `enableRtx: true` even though no RTX codec is on offer: it keeps the `nack` feedback on audio consumers,
  which mediasoup otherwise strips, and without it Opus is never resent.

## Transport

```js
const transport = await router.createPlainTransport({
  listenInfo: { protocol: 'udp', ip: '0.0.0.0', announcedAddress: sfuPublicIp },
  rtcpMux: false,
  comedia: false,
});
const consumer = await transport.consume({ producerId, rtpCapabilities, paused: true, enableRtx: true });
const { ip, port, rtcpPort } = await recorder.request('capture.allocateTrack', { ..., rtpParameters: consumer.rtpParameters });
await transport.connect({ ip, port, rtcpPort });
await consumer.resume();
if (consumer.kind === 'video') await consumer.requestKeyFrame();
await recorder.request('capture.trackConnected', { recordingId, trackId, consumerId: consumer.id, rtcp: { ip: transport.tuple.localIp, port: transport.rtcpTuple.localPort } });
```

`comedia: false` on a consume-only transport is the right setting; mediasoup then accepts RTCP only from the tuple given
to `connect()`. That is why the recorder sends its RTCP from the very socket it receives RTCP on: RTCP from an ephemeral
port is discarded silently and no NACK ever counts.

Pin the top layer on video consumers, `setPreferredLayers({ spatialLayer: 2, temporalLayer: 2 })`, and make sure nothing
else in your controller lowers it for this consumer.

If the producer lives on another router (another worker or host), pipe it to the router the recorder consumes from first
(`router.pipeToRouter`).

## Keyframes

Answer `needKeyFrame` with `consumer.requestKeyFrame()`. The recorder asks every second from connect and after every
unrecovered gap until frames reach the file, because one request is often not answered: at connect the consumer may not
be flowing yet, and after a loss a browser has been seen to take ten seconds.

## The layer-sync hole

mediasoup 3.25 and later aligns the layers of a simulcast producer through a remote clock estimate that needs three
Sender Reports from the browser. Until it has them, a consumer that wants a layer other than the one it started on drops
that layer's keyframe and forwards nothing at all: a hole of one to three seconds at the start of every consumer created
shortly after its producer (late joiners, camera toggles). With a small jitter buffer the next packet after the hole is
declared lost and the late keyframe discarded, and the camera stays frozen until the next chase. The recorder's default
3 s jitter buffer (`RECORDER_JITTER_BUFFER_MS`) rides through it; the keyframe chase covers anything longer.

## Payload types and SSRCs

They are mediasoup's choice on the consumer (VP8 is typically 101, Opus 100), never the endpoint's
`preferredPayloadType`. Send `consumer.rtpParameters` verbatim in `capture.allocateTrack`; the recorder builds its caps
from them.

## Sender reports

mediasoup's Sender Reports were consistent across audio and video to within 15 ms in every configuration measured. The
recorder keeps every one (`senderReports` in the manifest) and compose aligns tracks from them.

## Events worth forwarding

From your room logic, as `capture.event` lines: `peer.joined` and `peer.left` (every connection is its own peer),
`track.paused` / `track.resumed` on producer pause and resume (mute, camera off), `track.stopped` on producer close,
`share.started` / `share.stopped` for screen producers, `speaker.changed` from an `ActiveSpeakerObserver`, `hand.raised`
/ `hand.lowered`, `peer.hold` / `peer.unhold`, and `chat.message` if you have it. The layout only needs `peerId` and
`trackId` on them.

## Seen once, not explained

In one test run a camera consumer created for a second recording in the same room sent RTCP but no RTP for the whole
recording, while mediasoup reported its layer changes normally. It did not recur. If you see a track with sender reports
and zero packets in the manifest, that is what it looks like; logging the consumer's `layerschange` and `score` events
on your side is the cheapest way to have mediasoup's view of it when it happens.
