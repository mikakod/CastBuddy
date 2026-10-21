# CastBuddy

A private stream server you run yourself. Broadcast to it any way you like
(OBS, VLC, a file straight from the dashboard), then hand friends a **hidden
link** to watch, or download streams/files.    
No accounts, no public exposure unless you choose it.    
Can be run fully locally, or with the file/stream-sharing server public facing.   
Could also(with some work) be redesigned as a livestreaming/streaming service.   
Restreaming to YouTube/Twitch or another custom RTMPS server is also possible.   


```
Sources                        CastBuddy                        Outputs
+------+  rtmp   +--------------------------------+   +-- external RTMP (YouTube/Twitch/custom)
| OBS  +-------->|  MediaMTX ingest              |-->|  HLS (.m3u8) -> browsers & players
| VLC  |         |  built-in player (mpv)        |   |  WebRTC preview in dashboard
| file |         |  ffmpeg fan-out (one/dest)    |   +--
+------+         +--------------------------------+
```

## Why VLC stays silent but the stream doesn't

VLC's `--sout` taps the *decoded* output, not your speakers. The dashboard
gives you a ready-made command:

```
vlc "video.mkv" -A dummy --sout "#transcode{...}:std{access=rtmp,mux=flv,url=rtmp://localhost:1935/ingest}"
```

`-A dummy` gives zero local sound and full-volume audio on the stream. No
virtual cables, no capture. The built-in player does the same trick for you
(see below).

## Quick start

Managed mode needs no Docker; the daemon spawns and supervises MediaMTX
itself:

```powershell
# dashboard assets (once)
cd web; npm install; npm run build; cd ..

# daemon (creates castbuddy.toml on first run)
cargo run
# open http://127.0.0.1:8080
```

Add destinations in the dashboard (persisted to `castbuddy.toml`), point OBS at
`rtmp://localhost:1935/ingest`, done.

Prefer containers? A multi-arch image (linux/amd64 + linux/arm64, Apple
Silicon included) is ready - see [Running with Docker](#running-with-docker).

Don't want to build from source at all? The
[releases page](https://github.com/mikakod/CastBuddy/releases/latest) carries
ready-made Windows and Linux binaries with a starter config.

## Running with Docker

The same daemon, packaged for **linux/amd64 and linux/arm64** (Docker
Desktop on macOS and Windows runs the Linux image; Apple Silicon is native
arm64). MediaMTX, ffmpeg and mpv are bundled - only media and state live
outside the image:

```bash
docker compose -f deploy/docker-compose.yml up -d --build
# dashboard:  http://127.0.0.1:8080   (published host-loopback only)
# shares:     http://<host>:49989/watch/<key>/
# media:      drop files into deploy/data/media/
# state:      deploy/data/  (castbuddy.toml, logs/, mediamtx.generated.yml)
```

Every version is also published as a ready-made multi-arch image, so the
build step can be skipped:

```bash
docker pull ghcr.io/mikakod/castbuddy:latest
docker run -d --name castbuddy -p 127.0.0.1:8080:8080 -p 49989:49989 \
  -v castbuddy-data:/data -v "$PWD/media":/media:ro \
  ghcr.io/mikakod/castbuddy:latest
```

To point the compose file at the published image instead of building, set
`image: ghcr.io/mikakod/castbuddy:latest` and delete the `build:` block.

First run writes `deploy/data/castbuddy.toml` with container defaults
(loopback-inside-the-container binds, `/media` as the library); edit it and
restart to change settings - it survives image upgrades. The compose file
is hardened: `read_only` rootfs, tmpfs `/tmp`, `no-new-privileges`,
`cap_drop: ALL`, 2g memory limit, non-root user.

Notes:

- Only the share port is published; the dashboard is pinned to
  host-loopback. MediaMTX's ports stay loopback *inside* the container.
- **Publishing with OBS**: ingest is loopback-inside the container by
  default (safest). To stream from the host or LAN, set
  `rtmp_bind = "0.0.0.0"` in `deploy/data/castbuddy.toml` and uncomment
  the `1935:1935` port line in the compose file - cleartext RTMP, fine
  on a trusted LAN (the same trade-off as the native loopback/LAN
  setup). Beyond a trusted network use RTMPS + `1936/tcp` (below).
- Linux hosts: if the container cannot write `deploy/data/`, run
  `chown -R 1000:1000 deploy/data` (the image runs as uid 1000).
- The WebRTC dashboard preview needs UDP 8189 end-to-end, which bridged
  Docker networking cannot provide - the dashboard automatically falls
  back to the HLS preview (proxied, works everywhere). On Linux,
  `network_mode: host` gives you full WebRTC.
- Remote publishers: publish `1936/tcp`, mount your certs, set
  `tls_cert`/`tls_key` + `rtmps_bind = "0.0.0.0"` in the container config
  (see the comments in `deploy/castbuddy.docker.toml`).

## The built-in player

No OBS, no VLC; pick a file in the dashboard and it goes live:

- mpv encodes headless, a Rust pacer re-times the stream to realtime, ffmpeg
  relays it into RTMP as path `player` (destinations and HLS pick it up
  automatically, same as `ingest`).
- **Pause / resume / seek** without restarting the pipeline.
- **Monitoring toggle**: ON plays the file through your speakers via a second,
  display-only mpv instance; OFF is fully silent locally, the stream always
  keeps full volume.
- A self-healing watchdog restarts the encoder if anything dies, resuming at
  the current position.

Requires an `mpv` binary (PATH, or set `server.mpv`). `ffmpeg` likewise.

Same thing over the API:

```powershell
curl -X POST localhost:8080/api/v1/player/play -d '{"file":"party/night.mp4","monitor":false}'
curl -X POST localhost:8080/api/v1/player/pause
curl -X POST localhost:8080/api/v1/player/seek -d '{"position":120}'
curl -X POST localhost:8080/api/v1/player/resume
curl -X POST localhost:8080/api/v1/player/stop
curl localhost:8080/api/v1/player/status     # active, paused, time_pos, duration, monitor, error
curl "localhost:8080/api/v1/player/browse?dir="   # file browser (confined to media_dir)
```

Relative paths resolve against `server.media_dir`; absolute paths work too.

## Watching output

- **Dashboard preview**: WebRTC (WHEP), low latency, one click. WebRTC cannot
  carry H264 with B-frames (and never carries OBS's AAC audio). If MediaMTX
  tears the session down, the dashboard auto-switches to an HLS preview
  (video + audio, tolerant of any encoder tuning). For a pure low-latency
  WebRTC preview, disable B-frames in OBS: x264 encoder settings `bf=0`
  (or tune `zerolatency`), NVENC `BFrames=0`.
- **HLS**: `http://<host>:8888/<path>/index.m3u8` where `<path>` is `ingest`
  (OBS) or `player` (built-in player). VLC, mpv, browsers, in-game players.
- **Destinations**: the real output. Relays survive hiccups with exponential
  backoff and reconnect on their own.

## Configuration (`castbuddy.toml`)

Everything is optional; defaults work out of the box:

```toml
[server]
bind = "127.0.0.1"     # keep local; no auth on the API (Host pinned to loopback)
port = 8080
media_dir = "."        # file browser root for the built-in player
# mpv = "mpv"          # mpv binary (player); e.g. "bin/mpv/mpv.exe"
# ffmpeg = "ffmpeg"    # ffmpeg binary (fan-out + player relay)
# Share links (watch pages + file downloads) get their own listener so the
# dashboard can stay local-only. Leave it loopback and reverse-proxy it, or
# bind it wide if you want direct LAN/internet access to shares:
# public_bind = "0.0.0.0"
# public_port = 49989
# Rewrite the share links the dashboard shows (browser page, .m3u8 for
# VLC/mpv/...) with a fixed public base URL instead of the hostname you
# browse the dashboard from, for Caddy / Tailscale Serve in front of the
# share listener. Scheme defaults to http:// if omitted; include
# https:// when a TLS reverse proxy (Caddy, Tailscale) fronts it:
# public-url-enabled = true     # or yes/no
# public-url = "https://stream.example.com"
# Write the access log (viewer IPs, allow/deny, stream start/end, MediaMTX
# console lines). Set false to record nothing about who visited - e.g. for
# GDPR; rate limiting and all access rules stay active either way.
# enable-logging = true         # default; or yes/no

[mediamtx]
mode = "managed"       # "managed" = daemon spawns mediamtx; "external" = Docker/already-running
api_url = "http://127.0.0.1:9997"
rtmp_url = "rtmp://127.0.0.1:1935"
# binary = "mediamtx"  # mediamtx binary for managed mode (PATH, or a path)
# public_host = "stream.example.com"  # hostname used in generated publisher URLs
#                          # (empty = hostname of public-url, then rtmp_url host)
# strict = true          # only ingest/player/stream-key paths exist (no wildcard publish)
# tls_cert = "cert.pem"  # enabling both turns on RTMPS on :1936; a non-loopback
# tls_key  = "key.pem"   # rtmps_bind then also pins cleartext :1935 to loopback
# rtmps_bind = "0.0.0.0" # RTMPS bind (empty = rtmp_bind). "0.0.0.0" = guest OBS
#                        # over the internet may publish, RTMPS only

# Add destinations in the dashboard (persisted here), e.g.:
# [[destinations]]
# id = "youtube"
# name = "YouTube"
# url = "rtmp://a.rtmp.youtube.com/live2"
# key = "xxxx-xxxx"
# enabled = true

# Publish (stream) keys are managed in the dashboard's "Stream keys" card
# and persisted here:
# [[publish_keys]]
# id = "guest-obs"
# name = "Guest OBS"
# path = "guest-obs"
# key = "K3QX5..."   # 256-bit secret; MediaMTX only sees its SHA-256 hash
# created = 1792531965
# enabled = true

# Share links are managed in the dashboard's "Share links" card:
# [[shares]]
# id = "K7WQ2M..."   # 128-bit secret; the URL is /watch/<id> or /d/<id>
# name = "movie-night"
# kind = "live"      # "live" = watch page for a MediaMTX path,
#                    # "file"/"folder" = download(s) under media_dir
# target = "ingest"  # MediaMTX path (live) or media_dir-relative path
# expires = 0        # unix seconds; 0 = never expires
# created = 1792534654
```

Managed mode writes `mediamtx.generated.yml` with safe defaults: publish from
localhost/private LAN is allowed **only to the well-known `ingest` and `player`
paths** (VLC, the built-in player, fan-out tests); every stream-key path
requires its key from anywhere, including the same machine. Everyone else is
read-only. Each key gets its own MediaMTX path and a ready-made URL. **RTMP
credentials must ride on the playpath query**; MediaMTX's RTMP server reads
`user`/`pass` only from there, userinfo in the URL is discarded and OBS's
"Use authentication" checkbox is ineffective. The dashboard therefore hands
out an OBS split of **Server** = `rtmp[s]://host:port` (plain) and
**Stream Key** = `path?user=<id>&pass=<key>` (leave OBS's auth checkbox off),
plus a combined URL of the same query form for ffmpeg/VLC.
`strict = true` removes the wildcard path entirely; only `ingest`, `player`
and key paths exist, unknown path names are refused. `tls_cert` + `tls_key`
(PEM) enable RTMPS on :1936; restart MediaMTX (dashboard button) after changing
TLS settings. When the RTMPS listener is exposed (`rtmps_bind` non-loopback),
cleartext :1935 is automatically pinned to loopback, so non-localhost ingests
must be RTMPS and stream keys never cross the network in the clear.
Stream-key URLs handed to guests use the `public-url` hostname (or
`public_host`) with `rtmps://…:1936` when TLS is on. MediaMTX loads the cert
at startup; after the certificate is renewed (e.g. by Caddy), restart
MediaMTX so RTMPS serves the new cert.

## Ports and exposure

Security posture out of the box: everything is loopback-only except the
share listener:

| Address                        | What                                          | Expose? |
|--------------------------------|-----------------------------------------------|---------|
| `127.0.0.1:8080`               | dashboard, REST API, preview/WHEP proxy       | never   |
| `127.0.0.1:49989`              | share server (`/watch/…`, `/d/…`)             | **the only public thing** (`public_bind`/`public_port`) |
| `127.0.0.1:1935/8888/8889/9997` | MediaMTX RTMP / HLS / WebRTC / API (managed) | never   |
| UDP `127.0.0.1:8189`           | WebRTC ICE/DTLS (`webrtcLocalUDPAddress`)     | never   |

MediaMTX's unused protocols (RTSP, SRT, RIST, on-demand playback) are
explicitly disabled in the generated config; MediaMTX enables RTSP and SRT
*by default*, which would otherwise offer unauthenticated playback on every
interface. The managed config binds RTMP/HLS/WebRTC through `rtmp_bind`,
`hls_bind` and `webrtc_bind` in `[mediamtx]` (default `127.0.0.1`); the
control API is always loopback.

To accept **remote publishers** (a guest OBS over the internet): configure
`tls_cert`/`tls_key` for RTMPS and set `rtmps_bind = "0.0.0.0"`, forward only
TCP 1936 (router + firewall), and set `strict = true`. Cleartext :1935 stays
loopback-only, so remote publishers are forced onto encrypted RTMPS and stream
keys are never sent in the clear. Publish keys are required from everywhere;
dashboard, HLS and WebRTC stay loopback. Any TLS cert works; reusing a Let's
Encrypt cert obtained by a reverse proxy (Caddy) means guest OBS validates
the chain without extra setup.

**Docker**: run CastBuddy (MediaMTX runs as its child in managed mode, so it
needs no ports of its own) with only the share port published (`49989:49989`),
plus optionally a host-loopback dashboard mapping `127.0.0.1:8080:8080`.
Harden the container: `read_only: true` (with a writable volume for
`castbuddy.toml` + `media_dir`), `no-new-privileges`, `cap_drop: [ALL]`, a
memory limit, non-root. Docker is strong *defense in depth*; an attacker who
exploits the share server gets a throwaway filesystem with no credentials
beyond link keys, but it is not an absolute boundary (kernel escape bugs
exist), which is why the daemon never listens publicly in the first place and
why the share server only ever serves unguessable-link-scoped content
confined to `media_dir`.

## Logs

Everything goes to stdout **and** to `logs/castbuddy.log` (daily-rolling,
no retention limit, prune it if it grows). Useful lines:

| Target   | What it tells you                                                        |
|----------|--------------------------------------------------------------------------|
| `access` | viewer IPs: `opened watch`, `watching`, `download`, `browse`; `stream started/ended` with publisher IP and duration; denied (unknown/expired) share keys; rate-limit hits; dropped viewer connections |
| `mediamtx` | MediaMTX's own output (auth decisions, publishers connecting)         |
| `castbuddy` | lifecycle: config loaded, listeners bound, child process exits, shutdown |

Viewer sessions are deduplicated (first event per IP+share every 5 minutes;
playlist/segment polling stays at debug), so a live stream produces a handful
of lines per viewer, not thousands. Filter for just the interesting stuff:

```powershell
Select-String -Path logs\castbuddy.log -Pattern "access"   # who touched what
Select-String -Path logs\castbuddy.log -Pattern "WARN|ERROR"
```

The share listener rate-limits each IP to ~25 req/s (burst 60), generous for
any player and fatal for floods; offenders get `429` and one warn line per
five minutes.

### Turning the access log off (GDPR)

`enable-logging = false` under `[server]` (default `true`) silences the
whole `access` target **and** MediaMTX's console passthrough - the only two
places a visitor's IP can end up in a record. Nothing about *who* connected
is written anywhere (stdout or file). Enforcement is unchanged: rate
limiting, key checks and every other access rule stay fully active, only
the records are gone. Lifecycle lines (target `castbuddy`) keep flowing, so
the daemon stays troubleshootable. Takes effect on restart.

**Disclaimer:** if you serve this to the public internet, especially in the
EU/EEA, GDPR is on you. The access log records viewer IP addresses, which are
personal data, so you need a lawful basis, a retention limit, and a notice
your visitors can actually see (for example on the watch page or your site's
front page). Alternatively, set `enable-logging = false`: then no visitor
addresses are recorded at all, and no notice is required on that account.

## HTTPS / reaching links outside your LAN

Share links (`/watch/<key>`, `/d/<key>`) are the intended way to hand a stream
or a file to someone: the URL contains 128 bits of CSPRNG entropy, links can
expire and be revoked from the dashboard, and they're served on their own
listener (`public_bind`:`public_port`, default loopback:49989) so the
unauthenticated admin API never has to leave your machine.

What each link form is for:

- `/watch/<key>`: browser page (hls.js; native HLS on Safari).
- `/watch/<key>/index.m3u8`: raw HLS playlist for VLC, mpv, smart TVs and
  other non-browser players.
- `/d/<key>/<file>`: file download or in-browser stream with Range support.

**Non-browser players need the `.m3u8` URL**; VLC can't play the HTML page.
Some players require HTTPS. Pick one:

1. **Tailscale Serve, no open ports, no domain**: install Tailscale on the
   server, then

   ```
   tailscale serve --bg --https=443 http://127.0.0.1:49989
   ```

   Every device in your tailnet can now use
   `https://<server>.<tailnet>.ts.net/watch/<key>/index.m3u8`, with a real
   certificate Tailscale obtains and renews. Nothing is exposed to the
   internet and no router port is opened.
2. **Caddy + a domain** (public links): point a DNS A/AAAA record at your
   home IP (dynamic DNS is fine) and run

   ```
   your.domain {
       reverse_proxy 127.0.0.1:49989
   }
   ```

   Caddy obtains and renews the Let's Encrypt certificate automatically;
   links become `https://your.domain/watch/<key>/index.m3u8` for anyone.
3. **Direct port forward** (non-CGNAT, plain http): forward only
   `public_port`. Everything else is loopback-bound (see "Ports and
   exposure"), never expose 8080, MediaMTX's ports or UDP 8189.

The dashboard builds share URLs from the hostname you browse it from, so with
a proxy or Serve in place, swap the host in the copied link (or browse the
dashboard through that host so links are generated with it).

## Compatibility

| Platform | How |
|----------|-----|
| Windows  | native (`cargo build`, mpv/ffmpeg on PATH) - primary dev target |
| Linux    | native (mpv/ffmpeg from the distro), or the Docker image (amd64/arm64) |
| macOS    | native (`brew install mpv ffmpeg`), or Docker Desktop (arm64 on Apple Silicon) |

The daemon is portable Rust: the only platform splits are `cfg(unix)` /
`cfg(windows)` for the mpv IPC sockets and process handling, and there are
no platform-specific dependencies. The container image is built for
linux/amd64 and linux/arm64.

## Repository layout

```
crates/castbuddy/   Rust daemon (axum API + player + fanout supervisor + WHEP proxy)
web/                React dashboard (Vite)
deploy/             Docker: hardened compose, container config template, entrypoint
Dockerfile          multi-arch image build (linux/amd64 + linux/arm64)
.github/            GitHub Actions: CI on every push, release pipeline on v* tags
```

## Security notes

- Plain RTMP is **cleartext**; anyone able to watch your packets can rebuild
  the video. Over the internet use RTMPS or a VPN (e.g. Tailscale); plain RTMP
  is fine on a trusted LAN. CastBuddy enforces the split: while the RTMPS
  listener is exposed (`rtmps_bind` non-loopback) the cleartext :1935 listener
  is pinned to loopback, so every non-localhost ingest must be encrypted
  RTMPS. Note that whoever hosts the machine the stream terminates on could in
  principle read it; TLS protects the road, not the endpoint.
- Publish auth on MediaMTX: localhost + RFC1918 may publish **only to
  `ingest`/`player`** (VLC, built-in player); **stream-key paths need their key
  from anywhere**; 256-bit CSPRNG secrets, hashed (SHA-256) in the generated
  config, individually disable-able or revocable from the dashboard. Turn on
  `strict` when exposing the server so unknown path names are refused outright.
- Share links are the only thing served beyond localhost by default, and they
  hold 128 bits of CSPRNG entropy, so guessing one is not feasible. They can
  expire and be revoked instantly from the dashboard. The share listener is
  separate from the admin API; expose *it* (behind HTTPS), keep `bind` local.
  Client IPs come from the socket peer; the proxy-appended (last)
  `X-Forwarded-For` hop is honored only when the proxy connects from
  loopback (Caddy / Tailscale Serve on this machine), so a visitor cannot
  forge the address in the access log or dodge the per-IP rate limit by
  spoofing headers - on a directly-exposed listener the real socket
  address is always used.
- The dashboard/API has no authentication; keep `bind = "127.0.0.1"`. As
  defence in depth it rejects any request whose `Host` header is not a
  loopback name - that blocks DNS-rebinding attacks from websites - and the
  player only accepts files inside `server.media_dir` (absolute paths and
  `..`/symlink escapes are refused after canonicalization).
- File access through share links is confined to the share's target under
  `server.media_dir` (`..`, backslashes, drive letters and symlink escapes are
  rejected; paths are canonicalized before serving).
- Stream keys are stored in plaintext `castbuddy.toml`; keep the file private.
  The generated `mediamtx.generated.yml` contains only their SHA-256 hashes.

## Credits

CastBuddy wouldn't exist without these projects - thank you!

- [MediaMTX](https://github.com/bluenviron/mediamtx) - the RTMP/HLS/WebRTC
  streaming server CastBuddy manages; every publish and playback flows
  through it.
- [FFmpeg](https://github.com/FFmpeg/FFmpeg) - fan-out relays to external
  destinations and the built-in player's encoding pipeline.
- [mpv](https://github.com/mpv-player/mpv) - headless file playback for the
  built-in player.
- [hls.js](https://github.com/video-dev/hls.js) - HLS playback in browsers
  on the watch pages (pinned with an integrity hash).
- [Axum](https://github.com/tokio-rs/axum) and
  [Tokio](https://github.com/tokio-rs/tokio) - HTTP layer and async runtime
  behind the dashboard and share servers.
- [React](https://github.com/facebook/react) and
  [Vite](https://github.com/vitejs/vite) - the dashboard's frontend.
