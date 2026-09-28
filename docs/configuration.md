# Configuration

Everything comes from the environment. Only two variables are required.

## Required

| variable                | meaning                                                                                 |
| ----------------------- | --------------------------------------------------------------------------------------- |
| `RECORDER_ANNOUNCED_IP` | The address the SFU sends RTP to (returned in `capture.allocateTrack`)                  |
| `RECORDER_SECRET`       | Handshake secret controllers must present (`RECORDING_SECRET` is read as an older name) |

## Control socket and RTP

| variable                    | default                 | meaning                                                              |
| --------------------------- | ----------------------- | -------------------------------------------------------------------- |
| `RECORDER_ID`               | `$HOSTNAME`, `recorder` | This recorder's id in the registry and in every manifest             |
| `RECORDER_HOST`             | announced ip            | Address controllers connect to (written to the registry)             |
| `RECORDER_PORT`             | `3100`                  | Control socket and `/health`                                         |
| `RECORDER_LISTEN_HOST`      | `0.0.0.0`               | Listen address of the control socket                                 |
| `RECORDER_RTC_IP`           | `0.0.0.0`               | Listen address for RTP/RTCP                                          |
| `RECORDER_RTC_MIN_PORT`     | `41000`                 | UDP range for RTP/RTCP pairs; open it on the firewall, and with host |
| `RECORDER_RTC_MAX_PORT`     | `41999`                 | networking keep it clear of the SFU's own range                      |
| `RECORDER_JITTER_BUFFER_MS` | `3000`                  | Jitter buffer per track; see `mediasoup.md` for why it is this large |

## Spool and limits

| variable                    | default      | meaning                                                                    |
| --------------------------- | ------------ | -------------------------------------------------------------------------- |
| `RECORDER_SPOOL_DIR`        | `/spool`     | Where recordings are written (a volume)                                    |
| `RECORDER_MIN_FREE_BYTES`   | `5368709120` | Refuse a new capture below this much free space (5 GiB)                    |
| `RECORDER_MAX_RECORDING_MS` | `7200000`    | Hard cap on one recording (2 h); reaching it is a stop with `max_duration` |
| `RECORDER_ORPHAN_GRACE_MS`  | `30000`      | After losing the controller, how long before the capture is finalised      |

## Roles and compose

| variable                           | default                                           | meaning                                           |
| ---------------------------------- | ------------------------------------------------- | ------------------------------------------------- |
| `RECORDER_ROLE`                    | `both`                                            | `capture`, `compose` or `both`                    |
| `RECORDER_POLICY_DIR`              | `/etc/recorder/policies`                          | Layout policy files                               |
| `RECORDER_COMPOSE_WIDTH`           | `1280`                                            |                                                   |
| `RECORDER_COMPOSE_HEIGHT`          | `720`                                             |                                                   |
| `RECORDER_COMPOSE_FPS`             | `30`                                              |                                                   |
| `RECORDER_COMPOSE_VIDEO_KBPS`      | `2500`                                            |                                                   |
| `RECORDER_COMPOSE_AUDIO_KBPS`      | `128`                                             |                                                   |
| `RECORDER_COMPOSE_NICE`            | `15`                                              | CPU priority of a compose run                     |
| `RECORDER_COMPOSE_WHILE_CAPTURING` | `false`                                           | Compose while a capture is live on this recorder  |
| `RECORDER_COMPOSE_BRAND`           | empty                                             | Product name on the title card                    |
| `RECORDER_FONT`                    | `/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf` | Label font                                        |
| `RECORDER_COMPOSE_ICON_FONT`       | Material Icons in the Debian package              | Status icons; shapes are drawn when it is missing |

## Redis (optional)

Set `REDIS_HOST` to enable discovery, status and the compose queue (`protocol.md`).

| variable         | default | meaning                                           |
| ---------------- | ------- | ------------------------------------------------- |
| `REDIS_HOST`     | unset   | Enables Redis                                     |
| `REDIS_PORT`     | `6379`  |                                                   |
| `REDIS_PASSWORD` | unset   |                                                   |
| `REDIS_DB`       | `0`     | Also the channel prefix (`<db>:recording-events`) |
| `REDIS_TLS`      | `false` |                                                   |

## S3 (optional)

Set `S3_ENDPOINT` to upload finished recordings. Without it they stay in the spool.

| variable               | default     | meaning                                                         |
| ---------------------- | ----------- | --------------------------------------------------------------- |
| `S3_ENDPOINT`          | unset       | Enables upload; `http://` is allowed for private networks       |
| `S3_BUCKET`            | required    |                                                                 |
| `S3_REGION`            | `us-east-1` |                                                                 |
| `S3_ACCESS_KEY_ID`     | required    |                                                                 |
| `S3_SECRET_ACCESS_KEY` | required    |                                                                 |
| `S3_FORCE_PATH_STYLE`  | `true`      | Path-style requests (MinIO, Garage); `false` for virtual-hosted |

## Diagnostics

| variable             | default | meaning                                                                                                   |
| -------------------- | ------- | --------------------------------------------------------------------------------------------------------- |
| `RUST_LOG`           | `info`  | `tracing` filter                                                                                          |
| `RECORDER_TRACE_RTP` | `false` | Log every RTP packet at the socket and after the jitter buffer, and every frame written. Diagnostics only |

## Commands

- `recorder`: the service.
- `recorder compose-local <dir>`: compose a spool folder in place with no Redis or bucket. Reads the
  `RECORDER_COMPOSE_*`, `RECORDER_FONT` and `RECORDER_POLICY_DIR` variables with the same defaults.
- `recorder health`: the container healthcheck; exit 0 when `GET /health` on `RECORDER_PORT` answers 200.
