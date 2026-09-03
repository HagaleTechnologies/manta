# MAN-21: lowest-friction non-developer install path, independent of
# native packaging. Multi-stage: build with the full Rust toolchain +
# ALSA dev headers (cpal, manta-input's unconditional audio dependency,
# needs them at build time), ship only the built binary + runtime ALSA
# lib in a slim runtime image.
#
# `hpsdr` is enabled (pure UDP/std, no native dependency -- see
# crates/manta-input/src/hpsdr.rs's own module docs) matching the same
# feature set as the native release binaries (.github/workflows/release.yml).
# `soapy` is deliberately excluded -- see that workflow's comment for why.
#
# Built for linux/amd64 and linux/arm64 via `docker buildx build
# --platform linux/amd64,linux/arm64` (release.yml); a plain `docker build`
# on either architecture also works natively, cross-compiling nothing.

FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install --no-install-recommends -y \
    libasound2-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
RUN cargo build --release -p manta-cli --features hpsdr

FROM debian:bookworm-slim

RUN apt-get update && apt-get install --no-install-recommends -y \
    libasound2 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /home/manta manta

COPY --from=builder /build/target/release/manta /usr/local/bin/manta

USER manta
WORKDIR /home/manta

# Telnet DX cluster (:7300), JSON/WebSocket spot stream (:7301), Prometheus
# metrics (:7302) -- ARCHITECTURE.md §7/§8. Defaults; override via config.
EXPOSE 7300 7301 7302

ENTRYPOINT ["manta"]
CMD ["--help"]
