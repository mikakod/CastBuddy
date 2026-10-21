//! The public share server: unguessable live watch pages and file/folder
//! downloads on its own listener, so the admin dashboard/API can stay bound
//! to 127.0.0.1 while share links are reachable from elsewhere.
//!
//! Security model:
//! - a request only succeeds if the URL contains a valid, unexpired share id
//!   (128 bits of CSPRNG entropy - not guessable, revocable by deleting);
//! - file access is confined to the share's target directory under
//!   `server.media_dir` (component + canonicalize containment checks);
//! - live pages proxy MediaMTX's HLS endpoint, which listens on localhost.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_core::Stream;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::config::{Config, Share};
use crate::state::SharedState;

/// Cooldowns for the "first event per viewer session" dedup.
const PAGE_COOLDOWN: Duration = Duration::from_secs(300);
const MEDIA_COOLDOWN: Duration = Duration::from_secs(300);
const DENY_COOLDOWN: Duration = Duration::from_secs(300);
const LISTING_COOLDOWN: Duration = Duration::from_secs(60);

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/watch/{key}", get(watch_page))
        .route("/watch/{key}/", get(watch_page))
        .route("/watch/{key}/{file}", get(watch_proxy))
        .route("/d/{key}", get(download_root))
        .route("/d/{key}/{*rest}", get(download_rest))
        .layer(axum::middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

/// Per-IP token bucket in front of everything public: keeps a flood (or a
/// broken player) from drowning the logs or the machine.
async fn rate_limit(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let ip = crate::access::client_ip(request.headers(), Some(addr));
    if state.rate.allow(&ip).await {
        return next.run(request).await;
    }
    if state.access.first("ratelimit", &ip, "", DENY_COOLDOWN).await {
        tracing::warn!(target: "access", "share: rate limited ip={ip}");
    }
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "5")],
        "too many requests - slow down",
    )
        .into_response()
}

async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(request).await;
    let h = resp.headers_mut();
    h.entry(header::X_CONTENT_TYPE_OPTIONS)
        .or_insert(HeaderValue::from_static("nosniff"));
    h.entry("referrer-policy")
        .or_insert(HeaderValue::from_static("no-referrer"));
    h.entry("x-frame-options")
        .or_insert(HeaderValue::from_static("DENY"));
    resp
}

fn short(key: &str) -> String {
    if key.len() > 8 {
        format!("{}...", &key[..8])
    } else {
        key.to_string()
    }
}

/// Log a denied request at most once per IP per cooldown, then 404 like
/// everything else (no oracle for which part of the URL was wrong).
async fn deny(state: &SharedState, ip: &str, key: &str) -> Response {
    if state.access.first("denied", ip, key, DENY_COOLDOWN).await {
        tracing::warn!(target: "access", "share: ip={ip} denied unknown/expired key {}", short(key));
    }
    not_found()
}

/// The public root says what this server actually is and records what the
/// access log actually does - honest transparency instead of a fake
/// compliance banner. Matches the implementation exactly: no cookies, no
/// tracking, IP recording is the only personal data and can be disabled
/// (`enable-logging = false`).
async fn index() -> &'static str {
    "CastBuddy share server\n\
     \n\
     Shares are private links issued by the server owner:\n\
     /watch/<key> streams, /d/<key> delivers files. Without a valid link\n\
     there is nothing to see here.\n\
     \n\
     Privacy: opening a share link records your IP address in this\n\
     server's access log, used only for rate limiting and abuse handling.\n\
     No cookies, no tracking, no third-party content; nothing is sold or\n\
     shared. The server owner can disable IP recording entirely, in which\n\
     case no personal data is retained at all.\n"
}

fn find_share(cfg: &Config, key: &str) -> Option<Share> {
    cfg.shares
        .iter()
        .find(|s| s.id == key && !s.is_expired())
        .cloned()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

// ---------- live watch pages ----------

async fn watch_page(
    State(state): State<SharedState>,
    UrlPath((key,)): UrlPath<(String,)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = crate::access::client_ip(&headers, Some(addr));
    let cfg = state.config.read().await;
    let Some(share) = find_share(&cfg, &key) else {
        return deny(&state, &ip, &key).await;
    };
    if share.kind != "live" {
        return deny(&state, &ip, &key).await;
    }
    drop(cfg);
    if state.access.first("watch-page", &ip, &key, PAGE_COOLDOWN).await {
        tracing::info!(
            target: "access",
            "share: ip={ip} opened watch `{}` ({})", share.name, short(&key)
        );
    }
    let title = html_escape(&share.name);
    let page = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'unsafe-inline'; media-src 'self' blob:; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'">
<title>{title} - CastBuddy</title>
<style>
  html, body {{ margin: 0; height: 100%; background: #0b0d10; color: #e6e9ee; font-family: system-ui, sans-serif; }}
  video {{ width: 100%; height: 100%; object-fit: contain; background: #000; }}
  #msg {{ display: none; position: fixed; top: 45%; width: 100%; text-align: center; color: #9aa4b2; font-size: 14px; }}
  .tag {{ position: fixed; top: 10px; left: 12px; font-size: 12px; opacity: .55; pointer-events: none; }}
</style>
</head>
<body>
<video id="v" controls autoplay playsinline muted></video>
<div id="msg">Stream offline — retrying…</div>
<span class="tag">{title} &middot; CastBuddy</span>
<script src="https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js" integrity="sha384-cciJ0zi8d1uMKC2zJd7jvPY4HQt7W4ByUI/FlMkltvBi31aW61rcpVBhpmW8/NwX" crossorigin="anonymous"></script>
<script>
  (function () {{
    var video = document.getElementById("v");
    var msg = document.getElementById("msg");
    var src = location.pathname.replace(/\/+$/, "") + "/index.m3u8";
    function offline() {{
      msg.style.display = "block";
      setTimeout(boot, 5000);
    }}
    function boot() {{
      msg.style.display = "none";
      if (video.canPlayType("application/vnd.apple.mpegurl")) {{
        video.src = src;                       // Safari / native HLS
        video.onerror = offline;
      }} else if (window.Hls && Hls.isSupported()) {{
        var h = new Hls({{ maxBufferLength: 6, liveSyncDurationCount: 2, maxMaxBufferLength: 30 }});
        h.loadSource(src);
        h.attachMedia(video);
        h.on(Hls.Events.ERROR, function (_ev, data) {{
          if (data.fatal) {{
            try {{ h.destroy(); }} catch (e) {{}}
            offline();
          }}
        }});
      }} else {{
        document.body.insertAdjacentHTML("beforeend",
          "<p style='padding:1em'>Your browser can't play HLS. Open the .m3u8 link directly in VLC.</p>");
      }}
    }}
    boot();
  }})();
</script>
</body>
</html>
"#
    );
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(page),
    )
        .into_response()
}

/// Response body wrapper that logs when a client walks away mid-stream
/// (tab closed, VLC quit) instead of letting the disconnect vanish silently.
struct DropLog<S> {
    inner: S,
    tag: String,
    finished: bool,
}

impl<S> Stream for DropLog<S>
where
    S: Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    type Item = reqwest::Result<bytes::Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Err(e))) => {
                this.finished = true;
                tracing::warn!(target: "access", "share: upstream stream error ({}): {e}", this.tag);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl<S> Drop for DropLog<S> {
    fn drop(&mut self) {
        if !self.finished {
            tracing::info!(target: "access", "share: viewer connection closed ({})", self.tag);
        }
    }
}

async fn watch_proxy(
    State(state): State<SharedState>,
    UrlPath((key, file)): UrlPath<(String, String)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = crate::access::client_ip(&headers, Some(addr));
    if !is_safe_name(&file) {
        return not_found();
    }
    let cfg = state.config.read().await;
    let Some(share) = find_share(&cfg, &key) else {
        return deny(&state, &ip, &key).await;
    };
    if share.kind != "live" {
        return deny(&state, &ip, &key).await;
    }
    // Defense in depth: live targets are validated at creation, but the
    // config file is hand-editable - never let `..` or an all-dot name
    // reach the upstream URL.
    if share.target.contains("..") || share.target.trim_matches('.').is_empty() {
        return not_found();
    }
    let upstream = format!(
        "{}/{}/{}",
        cfg.mediamtx.hls_url.trim_end_matches('/'),
        share.target,
        file
    );
    if state.access.first("watch-media", &ip, &key, MEDIA_COOLDOWN).await {
        tracing::info!(
            target: "access",
            "share: ip={ip} watching `{}` ({}) [{}]", share.name, short(&key), file
        );
    }
    tracing::debug!("share-proxy {ip} {key}/{file} -> {upstream}");
    let resp = match state.http.get(&upstream).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("share-proxy fetch error: {e}");
            return not_found();
        }
    };
    if !resp.status().is_success() {
        tracing::warn!("share-proxy upstream status: {}", resp.status());
        return not_found();
    }
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let body = Body::from_stream(DropLog {
        inner: resp.bytes_stream(),
        tag: format!("ip={ip} share={}", short(&key)),
        finished: false,
    });
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response()
}

// ---------- file / folder downloads ----------

async fn download_root(
    State(state): State<SharedState>,
    UrlPath((key,)): UrlPath<(String,)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = crate::access::client_ip(&headers, Some(addr));
    let cfg = state.config.read().await;
    let Some(share) = find_share(&cfg, &key) else {
        return deny(&state, &ip, &key).await;
    };
    let media = PathBuf::from(&cfg.server.media_dir);
    match share.kind.as_str() {
        "file" => {
            let Some(path) = resolve_within(&media, &share.target).await else {
                return not_found();
            };
            if !path.is_file() {
                return not_found();
            }
            tracing::info!(target: "access", "share: ip={ip} download `{}` {}", share.name, share.target);
            serve_file(path, headers.get(header::RANGE)).await
        }
        "folder" => {
            let Some(dir) = resolve_within(&media, &share.target).await else {
                return not_found();
            };
            if !dir.is_dir() {
                return not_found();
            }
            if state.access.first("listing", &ip, &key, LISTING_COOLDOWN).await {
                tracing::info!(target: "access", "share: ip={ip} browse `{}` ({})", share.name, short(&key));
            }
            listing_response(&key, "", &dir, &share.name).await
        }
        // a live share linked as /d/<key> just forwards to the watch page
        "live" => {
            let dest = format!("/watch/{key}");
            (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, dest)]).into_response()
        }
        _ => not_found(),
    }
}

async fn download_rest(
    State(state): State<SharedState>,
    UrlPath((key, rest)): UrlPath<(String, String)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = crate::access::client_ip(&headers, Some(addr));
    let cfg = state.config.read().await;
    let Some(share) = find_share(&cfg, &key) else {
        return deny(&state, &ip, &key).await;
    };
    if share.kind != "folder" {
        return deny(&state, &ip, &key).await;
    }
    let media = PathBuf::from(&cfg.server.media_dir);
    let rel = if share.target.is_empty() {
        rest.clone()
    } else {
        format!("{}/{}", share.target.trim_end_matches('/'), rest)
    };
    let Some(path) = resolve_within(&media, &rel).await else {
        return not_found();
    };
    if path.is_dir() {
        if state.access.first("listing", &ip, &format!("{key}/{rel}"), LISTING_COOLDOWN).await {
            tracing::info!(target: "access", "share: ip={ip} browse `{}` {rel}", share.name);
        }
        let name = format!("{}/{}", share.name, rest);
        return listing_response(&key, &rest, &path, &name).await;
    }
    if path.is_file() {
        tracing::info!(target: "access", "share: ip={ip} download `{}` {rel}", share.name);
        return serve_file(path, headers.get(header::RANGE)).await;
    }
    not_found()
}

/// Resolve `rel` under `root`, rejecting any escape (also after symlink
/// resolution). Returns None on any failure.
async fn resolve_within(root: &Path, rel: &str) -> Option<PathBuf> {
    if !rel.is_empty()
        && rel
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == ".." || c.contains('\\') || c.contains(':'))
    {
        return None;
    }
    let canon_root = tokio::fs::canonicalize(root).await.ok()?;
    let joined = if rel.is_empty() {
        canon_root.clone()
    } else {
        root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
    };
    let canon = tokio::fs::canonicalize(joined).await.ok()?;
    if canon.starts_with(&canon_root) {
        Some(canon)
    } else {
        None
    }
}

async fn listing_response(key: &str, rel: &str, dir: &Path, title: &str) -> Response {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return not_found();
    };
    let mut items: Vec<(String, bool, u64)> = Vec::new();
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        // Same rule as the dashboard's file browser: hidden files (.DS_Store
        // and friends) are not part of the share's presentation.
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        items.push((name, is_dir, size));
    }
    items.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    let mut rows = String::new();
    if !rel.is_empty() {
        let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let href = if parent.is_empty() {
            format!("/d/{key}")
        } else {
            format!("/d/{key}/{}", percent_encode_path(parent))
        };
        rows.push_str(&format!(
            "<li><a href=\"{href}\">../</a></li>\n"
        ));
    }
    for (name, is_dir, size) in &items {
        let full_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let href = format!("/d/{key}/{}", percent_encode_path(&full_rel));
        let label = if *is_dir {
            format!("{}/", html_escape(name))
        } else {
            html_escape(name)
        };
        let meta = if *is_dir {
            String::new()
        } else {
            format!("<span class=\"m\">{}</span>", format_size(*size))
        };
        rows.push_str(&format!("<li><a href=\"{href}\">{label}</a>{meta}</li>\n"));
    }
    if items.is_empty() {
        rows.push_str("<li class=\"m\">(empty folder)</li>\n");
    }

    let title = html_escape(title);
    let page = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - CastBuddy</title>
<style>
  body {{ margin: 0; background: #0b0d10; color: #e6e9ee; font-family: system-ui, sans-serif; padding: 24px; }}
  h1 {{ font-size: 18px; font-weight: 600; }}
  ul {{ list-style: none; padding: 0; max-width: 720px; }}
  li {{ padding: 8px 10px; border-bottom: 1px solid #1c2128; display: flex; justify-content: space-between; gap: 12px; }}
  a {{ color: #7cc7ff; text-decoration: none; word-break: break-all; }}
  a:hover {{ text-decoration: underline; }}
  .m {{ color: #7d8590; font-size: 12px; white-space: nowrap; }}
</style>
</head>
<body>
<h1>{title}</h1>
<ul>
{rows}
</ul>
</body>
</html>
"#
    );
    Html(page).into_response()
}

/// Serve a file with `Accept-Ranges` and single-range support.
async fn serve_file(path: PathBuf, range: Option<&header::HeaderValue>) -> Response {
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return not_found();
    };
    let len = meta.len();
    let ct = content_type_for(&path).to_string();
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return not_found(),
    };

    let range = range
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range);

    match range {
        None => {
            let body = Body::from_stream(ReaderStream::new(file));
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, ct),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_LENGTH, len.to_string()),
                ],
                body,
            )
                .into_response()
        }
        Some((start, end)) => {
            if start >= len {
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{len}"))],
                )
                    .into_response();
            }
            let end = end.min(len - 1);
            let blen = end - start + 1;
            if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                return not_found();
            }
            let body = Body::from_stream(ReaderStream::new(file.take(blen)));
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, ct),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_LENGTH, blen.to_string()),
                    (header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}")),
                ],
                body,
            )
                .into_response()
        }
    }
}

/// Parse a single-range `bytes=start-end` or `bytes=start-` header.
fn parse_range(s: &str) -> Option<(u64, u64)> {
    let s = s.trim().strip_prefix("bytes=")?;
    if s.contains(',') {
        return None; // multi-range: not supported, fall back to full body
    }
    let (a, b) = s.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let end = if b.is_empty() {
        u64::MAX
    } else {
        b.parse().ok()?
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "ts" => "video/mp2t",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "ogv" => "application/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "gifv" => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// File names MediaMTX emits for HLS segments/init files.
fn is_safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '~'))
        && s != "."
        && s != ".."
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Encode each path segment, keeping `/` separators intact.
fn percent_encode_path(s: &str) -> String {
    s.split('/').map(percent_encode).collect::<Vec<_>>().join("/")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-99"), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-"), Some((100, u64::MAX)));
        assert_eq!(parse_range("bytes=5-3"), None);
        assert_eq!(parse_range("bytes=0-99,200-299"), None);
        assert_eq!(parse_range("items=0-1"), None);
    }

    #[test]
    fn safe_names_and_encoding() {
        assert!(is_safe_name("stream_0.m4s"));
        assert!(!is_safe_name("../x"));
        assert!(!is_safe_name("a/b"));
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode_path("a b/c d.mp4"), "a%20b/c%20d.mp4");
        assert_eq!(html_escape("<b>&'"), "&lt;b&gt;&amp;&#39;");
    }
}
