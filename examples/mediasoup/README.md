# mediasoup example

`recorder-client.mjs` is a controller you can copy into a mediasoup process: a socket.io connection to one recorder and
a `RecordingSession` that gives every producer a plain transport, a consumer and a track on the recorder, forwards the
room events, answers keyframe requests and stops cleanly. `docs/mediasoup.md` explains each choice.

`record.mjs` runs it end to end without a browser: a local mediasoup worker with two producers fed by `ffmpeg` (a test
card and a tone), fifteen seconds of capture, a stop.

```sh
# a recorder on this machine, capturing and composing into ./spool
docker run --rm --network host -e RECORDER_ANNOUNCED_IP=127.0.0.1 -e RECORDER_SECRET=change-me \
  -v "$PWD/spool:/spool" recorder

npm install
RECORDER_URL=http://127.0.0.1:3100 RECORDER_SECRET=change-me npm start
```

Then `spool/<recordingId>/` holds `tracks/ffmpeg/*.webm`, `events.jsonl`, `manifest.json` and, a few seconds later,
`composite.mp4`, `thumbnail.jpg` and `audio.m4a`.

On Linux, `--network host` lets the recorder's UDP range and the SFU reach each other on the loopback. On Docker Desktop
(macOS, Windows) host networking is not the host's loopback: run the recorder and this script on the same Linux VM, or
point `RECORDER_ANNOUNCED_IP` and `SFU_IP` at addresses both sides can reach.
