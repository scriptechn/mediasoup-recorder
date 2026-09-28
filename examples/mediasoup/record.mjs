// Records a router for a few seconds, end to end, with no browser: a mediasoup worker with two producers fed by
// ffmpeg over plain transports (a test card and a tone), the recorder capturing them, and a stop.
//
//   RECORDER_URL=http://127.0.0.1:3100 RECORDER_SECRET=change-me node record.mjs
//
// Needs `ffmpeg` on the PATH. Afterwards the recorder's spool holds <recordingId>/ with two tracks, the event log,
// the manifest and, a few seconds later, composite.mp4.

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import mediasoup from "mediasoup";
import { RecorderConnection, RecordingSession } from "./recorder-client.mjs";

const RECORDER_URL = process.env.RECORDER_URL ?? "http://127.0.0.1:3100";
const RECORDER_SECRET = process.env.RECORDER_SECRET ?? "change-me";
const SFU_IP = process.env.SFU_IP ?? "127.0.0.1";
const RECORD_SECONDS = Number(process.env.RECORD_SECONDS ?? 15);

const worker = await mediasoup.createWorker({ rtcMinPort: 30000, rtcMaxPort: 30999 });
const router = await worker.createRouter({
  mediaCodecs: [
    { kind: "audio", mimeType: "audio/opus", clockRate: 48000, channels: 2 },
    { kind: "video", mimeType: "video/VP8", clockRate: 90000 },
  ],
});

// Two producers fed by ffmpeg over plain transports with comedia (the SFU learns the source from the first packet).
async function produce(kind, ssrc, payloadType) {
  const transport = await router.createPlainTransport({
    listenInfo: { protocol: "udp", ip: SFU_IP },
    rtcpMux: false,
    comedia: true,
  });
  const producer = await transport.produce({
    kind,
    rtpParameters:
      kind === "audio"
        ? { codecs: [{ mimeType: "audio/opus", clockRate: 48000, channels: 2, payloadType }], encodings: [{ ssrc }] }
        : { codecs: [{ mimeType: "video/VP8", clockRate: 90000, payloadType }], encodings: [{ ssrc }] },
    appData: { source: kind === "audio" ? "mic" : "webcam" },
  });
  return { transport, producer };
}
const audio = await produce("audio", 1111, 100);
const video = await produce("video", 2222, 101);

const ffmpeg = spawn(
  "ffmpeg",
  [
    "-re",
    "-f",
    "lavfi",
    "-i",
    "testsrc=size=640x360:rate=25",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440",
    "-map",
    "0:v",
    "-c:v",
    "libvpx",
    "-b:v",
    "600k",
    "-deadline",
    "realtime",
    "-g",
    "25",
    "-f",
    "rtp",
    "-payload_type",
    "101",
    "-ssrc",
    "2222",
    `rtp://${SFU_IP}:${video.transport.tuple.localPort}?rtcpport=${video.transport.rtcpTuple.localPort}`,
    "-map",
    "1:a",
    "-c:a",
    "libopus",
    "-b:a",
    "64k",
    "-ac",
    "2",
    "-f",
    "rtp",
    "-payload_type",
    "100",
    "-ssrc",
    "1111",
    `rtp://${SFU_IP}:${audio.transport.tuple.localPort}?rtcpport=${audio.transport.rtcpTuple.localPort}`,
  ],
  { stdio: ["ignore", "ignore", "inherit"] },
);

// The recorder.
let session;
const recorder = new RecorderConnection({
  url: RECORDER_URL,
  secret: RECORDER_SECRET,
  clientId: "example",
  onNotification: (method, data) => {
    if (method === "needKeyFrame") session?.onNeedKeyFrame(data.trackId);
    if (method === "captureEnded") session?.onCaptureEnded();
  },
});
await recorder.ready;

session = new RecordingSession({
  recorder,
  router,
  sfuIp: SFU_IP,
  recordingId: randomUUID(),
  start: { title: "Example recording", startedByName: "record.mjs", metadata: { example: true } },
});
const peer = { peerId: "ffmpeg", name: "Test card" };
await session.begin([peer]);
await session.addProducer(peer.peerId, audio.producer, "mic");
await session.addProducer(peer.peerId, video.producer, "webcam");
session.event("speaker.changed", { peerId: peer.peerId });
console.log(`recording ${session.recordingId} for ${RECORD_SECONDS} s`);

await new Promise((r) => setTimeout(r, RECORD_SECONDS * 1000));
await session.stop("user");
console.log("stopped; the recorder is uploading or composing now");

ffmpeg.kill("SIGINT");
recorder.close();
worker.close();
