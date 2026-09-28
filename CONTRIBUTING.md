# Contributing

Issues and pull requests are welcome.

## Before you open a pull request

Run the one check list. It needs only Docker:

```sh
docker build --target build -t recorder-check .
docker run --rm -v "$PWD:/work" -w /work -e CARGO_TARGET_DIR=/tmp/target recorder-check ./scripts/check.sh
```

It runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`. CI runs the same script.

## What makes a change easy to accept

- One concern per pull request.
- A test for behaviour you change. The scene evaluation and the rasteriser are pure and cheap to test; the capture
  pipeline has a loopback test you can extend.
- A change to a contract updates the document that states it: `docs/protocol.md` for the control socket,
  `docs/spool-format.md` for the event log and the manifest, `docs/layout.md` for the policy, `docs/configuration.md`
  for a variable.
- Wire and manifest changes stay backward compatible: new fields are optional, old names keep an alias. Recordings made
  by an older version must still compose.

## Reporting a bug in a recording

The most useful report is the spool folder's `manifest.json` and `events.jsonl` (they hold no media), the recorder's log
for that recording, and the mediasoup version. Set `RECORDER_TRACE_RTP=true` to log every packet when the problem is
missing or frozen video.

## License

By contributing you agree that your contribution is licensed under Apache-2.0, as the rest of the project.
