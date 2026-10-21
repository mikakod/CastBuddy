# syntax=docker/dockerfile:1
#
# CastBuddy container - the same daemon as the native install, with
# MediaMTX, ffmpeg and mpv bundled. Multi-arch: linux/amd64 + linux/arm64
# (Apple Silicon via Docker Desktop).
#
#   docker build -t castbuddy:local .
#   docker run --rm -p 127.0.0.1:8080:8080 -p 49989:49989 \
#       -v ./data:/data -v ./media:/media:ro castbuddy:local
#
# deploy/docker-compose.yml wraps this with the hardened defaults.

### web dashboard ---------------------------------------------------------
FROM node:22-bookworm-slim AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

### rust daemon -----------------------------------------------------------
FROM rust:1-bookworm AS rust
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release -p castbuddy

### runtime ---------------------------------------------------------------
FROM debian:bookworm-slim
ARG TARGETARCH
ARG MEDIAMTX_VERSION=v1.11.3

# MediaMTX (managed child), ffmpeg (fan-out/player relay), mpv (headless
# encode player) - matching what the native install expects on PATH.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl ffmpeg mpv \
    && rm -rf /var/lib/apt/lists/* \
    && case "$TARGETARCH" in \
           amd64) MTX=amd64 ;; \
           arm64) MTX=arm64v8 ;; \
           *) echo "unsupported arch: $TARGETARCH" >&2; exit 1 ;; \
       esac \
    && curl -fsSL "https://github.com/bluenviron/mediamtx/releases/download/${MEDIAMTX_VERSION}/mediamtx_${MEDIAMTX_VERSION}_linux_${MTX}.tar.gz" \
        | tar -xz -C /usr/local/bin mediamtx

# Non-root user; /data holds config, logs and the generated MediaMTX
# config. WORKDIR is /data so the rolling log lands on the writable
# volume instead of the read-only image.
RUN useradd --uid 1000 --create-home castbuddy \
    && mkdir -p /data /media \
    && chown castbuddy:castbuddy /data

COPY --from=rust /src/target/release/castbuddy /usr/local/bin/castbuddy
COPY --from=web /src/web/dist /app/web/dist
COPY deploy/castbuddy.docker.toml /app/castbuddy.docker.toml
COPY deploy/entrypoint.sh /app/entrypoint.sh

USER castbuddy
WORKDIR /data
# The share server; the dashboard is published on host-loopback only by
# the compose file. MediaMTX's ports stay loopback inside the container.
EXPOSE 49989 8080
ENTRYPOINT ["/app/entrypoint.sh"]
