# syntax=docker/dockerfile:1
# Recorder: Rust + GStreamer 1.26 (Debian trixie). Run it with host networking: the RTP/RTCP port range binds on the
# host. Builds for linux/amd64 and linux/arm64.
FROM rust:1.97-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev pkg-config clang \
      gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock* rust-toolchain.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked 2>/dev/null || cargo build --release && cp target/release/recorder /recorder

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
      gstreamer1.0-plugins-ugly gstreamer1.0-libav ca-certificates fonts-material-design-icons-iconfont \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 1001 recorder \
    && mkdir -p /spool && chown recorder:recorder /spool
COPY --from=build /recorder /usr/local/bin/recorder
# Layout policies as data (docs/layout.md); fonts-dejavu-core (pulled in above) provides the label font, Material Icons the
# status icons.
COPY policies /etc/recorder/policies
USER recorder
ENV RECORDER_SPOOL_DIR=/spool RECORDER_POLICY_DIR=/etc/recorder/policies
CMD ["recorder"]
