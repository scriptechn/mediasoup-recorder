// A controller for the recorder, for mediasoup. Two classes:
//
//   RecorderConnection  one socket.io connection to one recorder (docs/protocol.md)
//   RecordingSession    one recording of one router: a plain transport and a consumer per producer,
//                       and the room events forwarded as event-log lines
//
// Copy it into your own SFU process and call the session's methods from your room logic.

import { io } from "socket.io-client";

const VIDEO_CODECS = ["video/vp8", "video/vp9", "video/h264"];
const AUDIO_CODECS = ["audio/opus"];

/**
 * What the recorder really receives (docs/mediasoup.md): the router's VP8/VP9/H264/Opus codecs with retransmission
 * and keyframe feedback declared, no RTX, no bandwidth-estimation feedback, only the orientation header extension.
 */
export function recorderRtpCapabilities(routerCapabilities) {
  const codecs = (routerCapabilities.codecs ?? [])
    .filter((c) => VIDEO_CODECS.includes(c.mimeType.toLowerCase()) || AUDIO_CODECS.includes(c.mimeType.toLowerCase()))
    .map((c) => ({
      ...c,
      rtcpFeedback:
        c.kind === "video"
          ? [{ type: "nack" }, { type: "nack", parameter: "pli" }, { type: "ccm", parameter: "fir" }]
          : [{ type: "nack" }],
    }));
  const headerExtensions = (routerCapabilities.headerExtensions ?? []).filter(
    (e) => e.uri === "urn:3gpp:video-orientation",
  );
  return { codecs, headerExtensions };
}

export class RecorderConnection {
  /**
   * @param {object} options
   * @param {string} options.url       e.g. http://10.0.0.5:3100
   * @param {string} options.secret    RECORDER_SECRET
   * @param {string} options.clientId  any id for this controller
   * @param {(method: string, data: any) => void} [options.onNotification]  needKeyFrame, captureEnded
   */
  constructor({ url, secret, clientId, onNotification, timeout = 5000 }) {
    this.timeout = timeout;
    this.ready = new Promise((resolve, reject) => {
      this.socket = io(url, { transports: ["websocket"], query: { clientId, secret }, reconnection: false });
      const timer = setTimeout(() => reject(new Error("timeout waiting for recorderReady")), timeout);
      this.socket.on("notification", ({ method, data }) => {
        if (method === "recorderReady") {
          clearTimeout(timer);
          resolve(data.load);
          return;
        }
        onNotification?.(method, data);
      });
      this.socket.on("connect_error", (e) => {
        clearTimeout(timer);
        reject(e);
      });
    });
  }

  /** `request` answered by `(serverError, response)`; rejects on serverError or timeout. */
  request(method, data) {
    return new Promise((resolve, reject) => {
      this.socket.timeout(this.timeout).emit("request", { method, data }, (timeoutError, serverError, response) => {
        if (timeoutError) return reject(timeoutError);
        if (serverError) return reject(new Error(`${method}: ${serverError}`));
        resolve(response);
      });
    });
  }

  close() {
    this.socket.close();
  }
}

export class RecordingSession {
  /**
   * @param {object} options
   * @param {RecorderConnection} options.recorder
   * @param {import('mediasoup').types.Router} options.router   the router the recorder consumes from
   * @param {string} options.sfuIp          the address the SFU listens on for the recorder's RTCP (listenInfo.ip)
   * @param {string} [options.sfuAnnouncedIp]  announcedAddress when the SFU is behind NAT
   * @param {string} options.recordingId
   * @param {object} [options.start]        anything else for capture.start: prefix, title, startedByName, metadata…
   */
  constructor({ recorder, router, sfuIp, sfuAnnouncedIp, recordingId, start = {} }) {
    this.recorder = recorder;
    this.router = router;
    this.sfuIp = sfuIp;
    this.sfuAnnouncedIp = sfuAnnouncedIp;
    this.recordingId = recordingId;
    this.start = start;
    this.rtpCapabilities = recorderRtpCapabilities(router.rtpCapabilities);
    this.tracks = new Map(); // producer id → { trackId, transport, consumer }
    this.stopped = false;
  }

  /** Open the spool on the recorder. `peers` are `{ peerId, name?, userId?, picture? }` already in the room. */
  async begin(peers = []) {
    await this.recorder.request("capture.start", {
      recordingId: this.recordingId,
      startedAt: Date.now(),
      policy: { name: "auto", version: 1 },
      peers,
      ...this.start,
    });
  }

  /**
   * A producer appeared (or existed at start): give it a transport, a consumer and a track on the recorder.
   * @param {string} peerId
   * @param {import('mediasoup').types.Producer} producer
   * @param {'mic'|'webcam'|'screen'} kind
   */
  async addProducer(peerId, producer, kind) {
    if (this.stopped || this.tracks.has(producer.id)) return;
    const track = { trackId: producer.id };
    this.tracks.set(producer.id, track);
    try {
      if (!this.router.canConsume({ producerId: producer.id, rtpCapabilities: this.rtpCapabilities }))
        throw new Error(`recorder cannot consume producer ${producer.id}`);

      track.transport = await this.router.createPlainTransport({
        listenInfo: { protocol: "udp", ip: this.sfuIp, announcedAddress: this.sfuAnnouncedIp },
        rtcpMux: false,
        comedia: false,
      });
      track.consumer = await track.transport.consume({
        producerId: producer.id,
        rtpCapabilities: this.rtpCapabilities,
        paused: true,
        enableRtx: true, // keeps `nack` on audio; no RTX codec is negotiated (docs/mediasoup.md)
      });
      if (track.consumer.kind === "video")
        await track.consumer.setPreferredLayers({ spatialLayer: 2, temporalLayer: 2 });

      const codec = track.consumer.rtpParameters.codecs[0];
      const { ip, port, rtcpPort } = await this.recorder.request("capture.allocateTrack", {
        recordingId: this.recordingId,
        trackId: track.trackId,
        peerId,
        kind,
        codec: codec.mimeType,
        clockRate: codec.clockRate,
        rtpParameters: track.consumer.rtpParameters,
      });
      await track.transport.connect({ ip, port, rtcpPort });
      await track.consumer.resume();
      if (track.consumer.kind === "video") await track.consumer.requestKeyFrame();

      await this.recorder.request("capture.trackConnected", {
        recordingId: this.recordingId,
        trackId: track.trackId,
        consumerId: track.consumer.id,
        rtcp: { ip: this.sfuAnnouncedIp ?? this.sfuIp, port: track.transport.rtcpTuple.localPort },
      });

      if (producer.paused) this.event("track.paused", { trackId: track.trackId, peerId });
      producer.on("pause", () => this.event("track.paused", { trackId: track.trackId, peerId }));
      producer.on("resume", () => this.event("track.resumed", { trackId: track.trackId, peerId }));
      producer.observer.once("close", () => this.removeProducer(peerId, producer, kind));
      if (kind === "screen") this.event("share.started", { peerId, trackId: track.trackId });
    } catch (error) {
      this.tracks.delete(producer.id);
      track.consumer?.close();
      track.transport?.close();
      throw error;
    }
  }

  async removeProducer(peerId, producer, kind) {
    const track = this.tracks.get(producer.id);
    if (!track) return;
    this.tracks.delete(producer.id);
    track.consumer?.close();
    track.transport?.close();
    if (this.stopped) return;
    if (kind === "screen") this.event("share.stopped", { peerId, trackId: track.trackId });
    this.event("track.stopped", { trackId: track.trackId, peerId });
    await this.recorder
      .request("capture.releaseTrack", { recordingId: this.recordingId, trackId: track.trackId })
      .catch(() => {});
  }

  /** One event-log line; the recorder stamps `t`. Fire and forget. */
  event(type, data) {
    if (this.stopped) return;
    this.recorder
      .request("capture.event", { recordingId: this.recordingId, event: { type, at: Date.now(), ...data } })
      .catch(() => {});
  }

  /** The recorder asked for a keyframe on a track (`needKeyFrame` notification). */
  async onNeedKeyFrame(trackId) {
    const track = [...this.tracks.values()].find((t) => t.trackId === trackId);
    if (track?.consumer?.kind === "video") await track.consumer.requestKeyFrame();
  }

  /** Stop from our side. The recorder ends its files while the streams still flow; the transports go after. */
  async stop(reason = "user") {
    if (this.stopped) return;
    this.stopped = true;
    try {
      await this.recorder.request("capture.stop", { recordingId: this.recordingId, reason });
    } finally {
      for (const track of this.tracks.values()) {
        track.consumer?.close();
        track.transport?.close();
      }
      this.tracks.clear();
    }
  }

  /** The recorder ended it on its own (`captureEnded` notification): nothing to send back. */
  onCaptureEnded() {
    if (this.stopped) return;
    this.stopped = true;
    for (const track of this.tracks.values()) {
      track.consumer?.close();
      track.transport?.close();
    }
    this.tracks.clear();
  }
}
