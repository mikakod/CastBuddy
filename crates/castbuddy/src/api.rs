use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::services::{ServeDir, ServeFile};

use crate::config::{Config, Destination};
use crate::state::SharedState;

/// Handlers here are all quick (local API + loopback MediaMTX); a ceiling
/// stops a wedged upstream from pinning dashboard requests open forever.
const API_TIMEOUT: Duration = Duration::from_secs(30);

pub fn router(state: SharedState, web_dist: String) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/config", get(get_config))
        .route("/destinations", get(list_destinations).post(create_destination))
        .route(
            "/destinations/{id}",
            axum::routing::put(update_destination).delete(delete_destination),
        )
        .route(
            "/destinations/{id}/enable",
            post(enable_destination).put(enable_destination),
        )
        .route("/destinations/{id}/disable", post(disable_destination))
        .route("/sources", get(sources))
        .route("/sources/vlc", get(vlc_command))
        .route("/publish-keys", get(list_publish_keys).post(create_publish_key))
        .route("/publish-keys/{id}", delete(delete_publish_key))
        .route("/publish-keys/{id}/enable", post(enable_publish_key))
        .route("/publish-keys/{id}/disable", post(disable_publish_key))
        .route("/shares", get(list_shares).post(create_share))
        .route("/shares/{id}", delete(delete_share))
        .route("/preview/{*path}", post(preview_whep))
        .route("/mediamtx/restart", post(restart_mediamtx))
        .route("/player/status", get(player_status))
        .route("/player/browse", get(player_browse))
        .route("/player/play", post(player_play))
        .route("/player/pause", post(player_pause))
        .route("/player/resume", post(player_resume))
        .route("/player/seek", post(player_seek))
        .route("/player/stop", post(player_stop))
        .route("/player/monitor", post(player_monitor))
        .layer(axum::middleware::from_fn(api_timeout))
        .layer(axum::middleware::from_fn(api_access_log));

    let api = api.with_state(state);

    let index_path = format!("{web_dist}/index.html");
    let static_files = ServeDir::new(&web_dist).fallback(ServeFile::new(index_path));

    Router::new()
        .nest("/api/v1", api)
        .fallback_service(static_files)
}

/// Handlers here are all quick (local API + loopback MediaMTX); a ceiling
/// stops a wedged upstream from pinning dashboard requests open forever.
async fn api_timeout(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    match tokio::time::timeout(API_TIMEOUT, next.run(request)).await {
        Ok(resp) => resp,
        Err(_) => {
            tracing::warn!(target: "access", "{method} {path} 504 (timed out after {API_TIMEOUT:?})");
            (StatusCode::GATEWAY_TIMEOUT, "upstream timed out").into_response()
        }
    }
}

/// Dashboard access log: every state-changing call at info with the caller
/// IP; the 2-second status poll stays at debug so the log remains readable.
async fn api_access_log(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let ip = crate::access::client_ip(&headers, Some(addr));
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let start = Instant::now();
    let resp = next.run(request).await;
    let status = resp.status();
    if path.ends_with("/status") {
        tracing::debug!(target: "access", "{method} {path} {status} ip={ip} {}ms", start.elapsed().as_millis());
    } else {
        tracing::info!(target: "access", "{method} {path} {status} ip={ip} {}ms", start.elapsed().as_millis());
    }
    resp
}

// ---------- status ----------

#[derive(Serialize)]
struct StatusReply {
    mediamtx_reachable: bool,
    mediamtx_error: Option<String>,
    mediamtx_mode: String,
    ingest_live: bool,
    ingest_path: Option<String>,
    ingest_viewers: Option<usize>,
    rtmp_url: String,
    /// Port of the public share listener (shares: /watch/… and /d/…).
    public_port: u16,
    /// Configured public base URL for share links (e.g. "http://example.com")
    /// when `public-url-enabled` is on; null = build links from the
    /// dashboard's own hostname + `public_port`.
    public_url: Option<String>,
    /// Port of MediaMTX's HLS listener - the browser preview falls back to
    /// HLS when WebRTC can't carry the stream (e.g. H264 with B-frames).
    hls_port: u16,
    destinations: Vec<DestinationStatus>,
}

#[derive(Serialize)]
struct DestinationStatus {
    id: String,
    name: String,
    url: String,
    key: String,
    enabled: bool,
    state: String,
    last_error: Option<String>,
    logs: Vec<String>,
}

async fn status(State(state): State<SharedState>) -> Json<StatusReply> {
    let cfg = state.config.read().await;
    let (ingest, mediamtx_error) = match state.mediamtx.active_ingest(&state.http).await {
        Ok(Some(p)) => (Some(p), None),
        Ok(None) => (None, None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let fanout = state.fanout.lock().await;

    let destinations = {
        let mut list = Vec::new();
        for d in &cfg.destinations {
            let (state_str, last_error, logs) = match fanout.get(&d.id) {
                Some(st) => (
                    st.status.as_str().to_string(),
                    st.last_error.clone(),
                    try_take_logs(&st.logs),
                ),
                None => ("stopped".to_string(), None, Vec::new()),
            };
            list.push(DestinationStatus {
                id: d.id.clone(),
                name: d.name.clone(),
                url: d.url.clone(),
                key: d.key.clone(),
                enabled: d.enabled,
                state: state_str,
                last_error,
                logs,
            });
        }
        list
    };

    Json(StatusReply {
        mediamtx_reachable: mediamtx_error.is_none(),
        mediamtx_error,
        mediamtx_mode: format!("{:?}", cfg.mediamtx.mode).to_lowercase(),
        ingest_live: ingest.is_some(),
        ingest_path: ingest.as_ref().map(|p| p.name.clone()),
        ingest_viewers: ingest.as_ref().and_then(|p| p.viewer_count()),
        rtmp_url: cfg.mediamtx.rtmp_url.clone(),
        public_port: cfg.server.public_port,
        public_url: cfg.server.public_base_url(),
        hls_port: cfg
            .mediamtx
            .hls_url
            .parse::<reqwest::Url>()
            .ok()
            .and_then(|u| u.port())
            .unwrap_or(8888),
        destinations,
    })
}

/// Cheap helper: snapshot the log buffer without blocking the runtime.
fn try_take_logs(buf: &crate::fanout::LogBuffer) -> Vec<String> {
    match buf.try_lock() {
        Ok(l) => l.iter().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

// ---------- config ----------

async fn get_config(State(state): State<SharedState>) -> Json<Config> {
    Json(state.config.read().await.clone())
}

// ---------- destinations ----------

async fn list_destinations(State(state): State<SharedState>) -> Json<Vec<Destination>> {
    Json(state.config.read().await.destinations.clone())
}

#[derive(Deserialize)]
struct DestinationInput {
    name: String,
    url: String,
    #[serde(default)]
    key: String,
}

fn validate(input: &DestinationInput) -> Result<(), (StatusCode, String)> {
    if input.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".into()));
    }
    if !input.url.starts_with("rtmp://") && !input.url.starts_with("rtmps://") {
        return Err((
            StatusCode::BAD_REQUEST,
            "url must start with rtmp:// or rtmps://".into(),
        ));
    }
    Ok(())
}

fn slugify(name: &str) -> String {
    let base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if base.is_empty() {
        "dest".into()
    } else {
        base
    }
}

async fn create_destination(
    State(state): State<SharedState>,
    Json(input): Json<DestinationInput>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    validate(&input)?;
    let mut cfg = state.config.write().await;
    let mut id = slugify(&input.name);
    let mut n = 2;
    while cfg.destinations.iter().any(|d| d.id == id) {
        id = format!("{}-{n}", slugify(&input.name));
        n += 1;
    }
    let dest = Destination {
        id,
        name: input.name.trim().to_string(),
        url: input.url.trim().to_string(),
        key: input.key.trim().to_string(),
        enabled: false,
    };
    cfg.destinations.push(dest.clone());
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok((StatusCode::CREATED, Json(dest)))
}

async fn update_destination(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(input): Json<DestinationInput>,
) -> Result<Json<Destination>, (StatusCode, String)> {
    validate(&input)?;
    let mut cfg = state.config.write().await;
    let dest = cfg
        .destinations
        .iter_mut()
        .find(|d| d.id == id)
        .ok_or((StatusCode::NOT_FOUND, "destination not found".into()))?;
    dest.name = input.name.trim().to_string();
    dest.url = input.url.trim().to_string();
    dest.key = input.key.trim().to_string();
    let out = dest.clone();
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(out))
}

async fn set_enabled(
    state: &SharedState,
    id: &str,
    enabled: bool,
) -> Result<Json<Destination>, (StatusCode, String)> {
    let mut cfg = state.config.write().await;
    let dest = cfg
        .destinations
        .iter_mut()
        .find(|d| d.id == id)
        .ok_or((StatusCode::NOT_FOUND, "destination not found".into()))?;
    dest.enabled = enabled;
    let out = dest.clone();
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(out))
}

async fn enable_destination(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Destination>, (StatusCode, String)> {
    set_enabled(&state, &id, true).await
}

async fn disable_destination(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Destination>, (StatusCode, String)> {
    set_enabled(&state, &id, false).await
}

async fn delete_destination(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut cfg = state.config.write().await;
    let before = cfg.destinations.len();
    cfg.destinations.retain(|d| d.id != id);
    if cfg.destinations.len() == before {
        return Err((StatusCode::NOT_FOUND, "destination not found".into()));
    }
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    drop(cfg);
    // Kill its ffmpeg immediately; supervisor would do it on next tick anyway.
    let mut fanout = state.fanout.lock().await;
    if let Some(mut st) = fanout.remove(&id) {
        if let Some(mut child) = st.child.take() {
            let _ = child.start_kill();
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------- sources ----------

#[derive(Serialize)]
struct SourcesReply {
    /// URL to paste into OBS as "Server" (scheme/port follow TLS settings).
    obs_url: String,
    obs_key_hint: String,
    /// Whether RTMPS (rtmps:// :1936) is enabled.
    rtmps: bool,
    /// Whether publishing requires a stream key (no wildcard paths).
    strict: bool,
    /// Whether a publisher is connected right now.
    ingest_live: bool,
    ingest_path: Option<String>,
}

async fn sources(State(state): State<SharedState>) -> Json<SourcesReply> {
    let cfg = state.config.read().await;
    let ingest = state
        .mediamtx
        .active_ingest(&state.http)
        .await
        .ok()
        .flatten();
    Json(SourcesReply {
        obs_url: cfg.mediamtx.local_ingest_url(),
        obs_key_hint: if cfg.mediamtx.strict {
            "publishing requires a Stream key".into()
        } else {
            "anything, e.g. castbuddy".into()
        },
        rtmps: cfg.mediamtx.tls_enabled(),
        strict: cfg.mediamtx.strict,
        ingest_live: ingest.is_some(),
        ingest_path: ingest.as_ref().map(|p| p.name.clone()),
    })
}

#[derive(Deserialize)]
struct VlcParams {
    file: Option<String>,
}

async fn vlc_command(
    State(state): State<SharedState>,
    Query(params): Query<VlcParams>,
) -> Json<serde_json::Value> {
    let cfg = state.config.read().await;
    // Well-known path so the snippet keeps working under strict mode. VLC
    // runs on this machine, so it always uses the local (cleartext) ingest.
    let url = format!("{}/ingest", cfg.mediamtx.local_ingest_url());
    let file = params
        .file
        .unwrap_or_else(|| "C:\\path\\to\\video.mkv".into());
    // -A dummy: no local audio; sout still carries full-volume audio.
    let command = format!(
        "vlc \"{file}\" -A dummy --sout \"#transcode{{vcodec=h264,vb=4000,scale=Auto,acodec=mp4a,ab=128,channels=2,samplerate=44100}}:std{{access=rtmp,mux=flv,url={url}}}\""
    );
    Json(serde_json::json!({ "command": command, "note": "VLC plays silently on your PC; the stream gets full-volume audio." }))
}

// ---------- webrtc preview (WHEP proxy) ----------

async fn preview_whep(
    State(state): State<SharedState>,
    Path(rest): Path<String>,
    body: String,
) -> Result<Response, (StatusCode, String)> {
    let cfg = state.config.read().await;
    let webrtc_base = cfg.mediamtx.webrtc_url.trim_end_matches('/').to_string();
    let url = format!("{webrtc_base}/{rest}/whep");
    drop(cfg);

    let resp = state
        .http
        .post(&url)
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(body)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("webrtc endpoint unreachable: {e}")))?;

    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let answer = resp
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("reading answer: {e}")))?;

    Ok((status, [(header::CONTENT_TYPE, "application/sdp")], answer).into_response())
}

// ---------- mediamtx management ----------

async fn restart_mediamtx(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if state.mode().await != crate::config::MediaMtxMode::Managed {
        return Err((
            StatusCode::BAD_REQUEST,
            "mediamtx is managed externally (docker?); restart it there".into(),
        ));
    }
    state.stop_mediamtx().await;
    state
        .start_mediamtx()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- publish (stream) keys ----------

#[derive(Serialize)]
struct PublishKeyReply {
    id: String,
    name: String,
    path: String,
    key: String,
    created: u64,
    enabled: bool,
    /// Ready-made publisher URL with credentials embedded (ffmpeg/VLC etc).
    obs_url: String,
    /// OBS wants the URL split: paste this into Settings → Stream → Server…
    obs_server: String,
    /// …and this into the "Stream Key" field. It carries the credentials as
    /// query args (?user=…&pass=…) because MediaMTX's RTMP auth reads them
    /// ONLY from the playpath query - userinfo in the Server URL and OBS's
    /// "Use authentication" checkbox are both discarded server-side.
    /// Leave OBS's "Use authentication" checkbox OFF.
    obs_key: String,
}

fn key_reply(cfg: &Config, k: &crate::config::PublishKey) -> PublishKeyReply {
    // Stream keys are for guest publishers over the internet, so they use the
    // public hostname (public-url / public_host) and RTMPS when TLS is on.
    let base = cfg
        .mediamtx
        .public_rtmp_url(cfg.server.public_url_hostname().as_deref());
    let (scheme, hostport) = base
        .split_once("://")
        .unwrap_or(("rtmp", "127.0.0.1:1935"));
    PublishKeyReply {
        id: k.id.clone(),
        name: k.name.clone(),
        path: k.path.clone(),
        key: k.key.clone(),
        created: k.created,
        enabled: k.enabled,
        // Credentials live in the playpath query: the only channel MediaMTX's
        // RTMP server reads (tcUrl userinfo is dropped; app-embedded queries
        // get mangled into the path).
        obs_url: format!(
            "{scheme}://{hostport}/{}?user={}&pass={}",
            k.path, k.id, k.key
        ),
        obs_server: format!("{scheme}://{hostport}"),
        obs_key: format!("{}?user={}&pass={}", k.path, k.id, k.key),
    }
}

async fn list_publish_keys(State(state): State<SharedState>) -> Json<Vec<PublishKeyReply>> {
    let cfg = state.config.read().await;
    Json(cfg.publish_keys.iter().map(|k| key_reply(&cfg, k)).collect())
}

#[derive(Deserialize)]
struct PublishKeyInput {
    name: String,
}

/// Persist config, rewrite the managed config file and push the auth
/// database to MediaMTX live (no restart; publishers stay connected).
async fn apply_auth(state: &SharedState) -> Result<(), (StatusCode, String)> {
    let cfg = state.config.read().await.clone();
    if cfg.mediamtx.mode == crate::config::MediaMtxMode::Managed {
        let yml = std::path::PathBuf::from(&cfg.mediamtx.config_path);
        std::fs::write(&yml, crate::config::generated_mediamtx_config(&cfg)).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("writing {}: {e}", yml.display()),
            )
        })?;
    }
    let users = crate::config::auth_internal_users(&cfg);
    if let Err(e) = state.mediamtx.patch_auth(&state.http, users).await {
        // Not fatal: the rewritten file applies on the next mediamtx start.
        tracing::warn!("live auth patch failed (applies after mediamtx restart): {e:#}");
    }
    Ok(())
}

async fn create_publish_key(
    State(state): State<SharedState>,
    Json(input): Json<PublishKeyInput>,
) -> Result<(StatusCode, Json<PublishKeyReply>), (StatusCode, String)> {
    let name = input.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "name must be 1-64 characters".into()));
    }
    let mut cfg = state.config.write().await;
    let base = slugify(name);
    let mut id = base.clone();
    let mut n = 2;
    while cfg.publish_keys.iter().any(|k| k.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    let key = crate::config::PublishKey {
        id: id.clone(),
        name: name.to_string(),
        path: id.clone(),
        key: crate::keys::secret(32),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        enabled: true,
    };
    let reply = key_reply(&cfg, &key);
    cfg.publish_keys.push(key);
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    drop(cfg);
    apply_auth(&state).await?;
    Ok((StatusCode::CREATED, Json(reply)))
}

async fn delete_publish_key(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut cfg = state.config.write().await;
    let before = cfg.publish_keys.len();
    cfg.publish_keys.retain(|k| k.id != id);
    if cfg.publish_keys.len() == before {
        return Err((StatusCode::NOT_FOUND, "unknown key".into()));
    }
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    drop(cfg);
    apply_auth(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_publish_key_enabled(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    enabled: bool,
) -> Result<Json<PublishKeyReply>, (StatusCode, String)> {
    let mut cfg = state.config.write().await;
    let pos = cfg
        .publish_keys
        .iter()
        .position(|k| k.id == id)
        .ok_or((StatusCode::NOT_FOUND, "unknown key".to_string()))?;
    cfg.publish_keys[pos].enabled = enabled;
    let reply = key_reply(&cfg, &cfg.publish_keys[pos]);
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    drop(cfg);
    apply_auth(&state).await?;
    Ok(Json(reply))
}

async fn enable_publish_key(
    state: State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<PublishKeyReply>, (StatusCode, String)> {
    set_publish_key_enabled(state, Path(id), true).await
}

async fn disable_publish_key(
    state: State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<PublishKeyReply>, (StatusCode, String)> {
    set_publish_key_enabled(state, Path(id), false).await
}

// ---------- share links ----------

#[derive(Serialize)]
struct ShareReply {
    id: String,
    name: String,
    kind: String,
    target: String,
    created: u64,
    expires: u64,
    /// URL path on the public listener (the dashboard prefixes host:port).
    path: String,
}

fn share_reply(s: &crate::config::Share) -> ShareReply {
    ShareReply {
        id: s.id.clone(),
        name: s.name.clone(),
        kind: s.kind.clone(),
        target: s.target.clone(),
        created: s.created,
        expires: s.expires,
        path: s.url_path(),
    }
}

async fn list_shares(State(state): State<SharedState>) -> Json<Vec<ShareReply>> {
    let cfg = state.config.read().await;
    Json(cfg.shares.iter().map(share_reply).collect())
}

#[derive(Deserialize)]
struct ShareInput {
    name: String,
    /// "live" | "file" | "folder"
    kind: String,
    /// MediaMTX path (live) or media_dir-relative path (file/folder).
    target: String,
    /// Unix seconds; 0 = never expires.
    #[serde(default)]
    expires: u64,
}

async fn create_share(
    State(state): State<SharedState>,
    Json(input): Json<ShareInput>,
) -> Result<(StatusCode, Json<ShareReply>), (StatusCode, String)> {
    let name = input.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "name must be 1-64 characters".into()));
    }
    let kind = input.kind.trim();
    if !matches!(kind, "live" | "file" | "folder") {
        return Err((
            StatusCode::BAD_REQUEST,
            "kind must be live, file or folder".into(),
        ));
    }
    let target = input.target.trim().trim_start_matches('/').to_string();
    if input.expires != 0 && input.expires <= crate::config::unix_now() {
        return Err((
            StatusCode::BAD_REQUEST,
            "expires must be in the future (0 = never)".into(),
        ));
    }

    let mut cfg = state.config.write().await;
    match kind {
        "live" => {
            if target.is_empty()
                || !target
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-./~".contains(c))
            {
                return Err((StatusCode::BAD_REQUEST, "invalid media path".into()));
            }
        }
        "file" | "folder" => {
            let media = std::path::PathBuf::from(&cfg.server.media_dir);
            let canon_root = tokio::fs::canonicalize(&media)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("media_dir: {e}"),
                    )
                })?;
            let joined = media.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
            let canon = tokio::fs::canonicalize(&joined).await.map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "target does not exist under media_dir".to_string(),
                )
            })?;
            if !canon.starts_with(&canon_root) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "target escapes media_dir".into(),
                ));
            }
            let want_dir = kind == "folder";
            if canon.is_dir() != want_dir {
                return Err((
                    StatusCode::BAD_REQUEST,
                    if want_dir {
                        "target is not a directory".into()
                    } else {
                        "target is not a file".into()
                    },
                ));
            }
        }
        _ => unreachable!(),
    }

    let share = crate::config::Share {
        id: crate::keys::secret(16),
        name: name.to_string(),
        kind: kind.to_string(),
        target,
        created: crate::config::unix_now(),
        expires: input.expires,
    };
    let reply = share_reply(&share);
    cfg.shares.push(share);
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok((StatusCode::CREATED, Json(reply)))
}

async fn delete_share(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut cfg = state.config.write().await;
    let before = cfg.shares.len();
    cfg.shares.retain(|s| s.id != id);
    if cfg.shares.len() == before {
        return Err((StatusCode::NOT_FOUND, "unknown share".into()));
    }
    cfg.save(&state.config_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- built-in player ----------

/// Read the bits of config the player needs.
async fn player_cfg(
    state: &SharedState,
) -> (String, String, String) {
    let cfg = state.config.read().await;
    (
        cfg.mediamtx.rtmp_url.clone(),
        cfg.server.ffmpeg.clone(),
        cfg.server.mpv.clone(),
    )
}

async fn player_status(State(state): State<SharedState>) -> Json<crate::player::PlayerStatus> {
    Json(state.player.status().await)
}

// ---------- file browsing (confined to server.media_dir) ----------

const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "mpg", "mpeg", "ts", "flv", "mp3", "wav",
    "flac", "ogg", "opus", "m4a", "aac",
];

fn is_media_file(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .map(|e| {
            let e = e.to_string_lossy().to_lowercase();
            MEDIA_EXTENSIONS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    dir: String,
}

#[derive(Serialize)]
struct BrowseEntry {
    name: String,
    is_dir: bool,
}

#[derive(Serialize)]
struct BrowseReply {
    /// Normalized relative directory ("" = root).
    dir: String,
    parent: Option<String>,
    entries: Vec<BrowseEntry>,
}

async fn player_browse(
    State(state): State<SharedState>,
    Query(q): Query<BrowseQuery>,
) -> Result<Json<BrowseReply>, (StatusCode, String)> {
    let root = {
        let cfg = state.config.read().await;
        std::fs::canonicalize(&cfg.server.media_dir).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("media_dir {:?} not accessible: {e}", cfg.server.media_dir),
            )
        })?
    };

    // Normalize the requested path; reject anything absolute or parent-escaping.
    let normalized = q.dir.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for comp in normalized.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                return Err((StatusCode::BAD_REQUEST, "path escapes media dir".into()));
            }
            c => parts.push(c),
        }
    }
    let cur = if parts.is_empty() {
        root.clone()
    } else {
        let candidate = root.join(parts.join("/"));
        let c = std::fs::canonicalize(&candidate)
            .map_err(|_| (StatusCode::BAD_REQUEST, "directory not found".into()))?;
        if !c.starts_with(&root) {
            return Err((StatusCode::BAD_REQUEST, "path escapes media dir".into()));
        }
        c
    };
    if !cur.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "not a directory".into()));
    }

    let read = std::fs::read_dir(&cur)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read_dir: {e}")))?;
    let mut entries: Vec<BrowseEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            entries.push(BrowseEntry { name, is_dir });
        } else if is_media_file(&name) {
            entries.push(BrowseEntry { name, is_dir: false });
        }
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));

    Ok(Json(BrowseReply {
        dir: parts.join("/"),
        parent: if parts.is_empty() {
            None
        } else {
            Some(parts[..parts.len() - 1].join("/"))
        },
        entries,
    }))
}

#[derive(Deserialize)]
struct PlayRequest {
    file: String,
    #[serde(default)]
    monitor: bool,
}

async fn player_play(
    State(state): State<SharedState>,
    Json(req): Json<PlayRequest>,
) -> Result<Json<crate::player::PlayerStatus>, (StatusCode, String)> {
    let (rtmp, ff, mpv) = player_cfg(&state).await;
    // Relative paths (what the file browser produces) resolve against media_dir.
    let file = {
        let p = std::path::Path::new(&req.file);
        if p.is_absolute() {
            req.file.clone()
        } else {
            let media_dir = state.config.read().await.server.media_dir.clone();
            std::path::PathBuf::from(media_dir)
                .join(&req.file)
                .to_string_lossy()
                .into_owned()
        }
    };
    state
        .player
        .play(&file, req.monitor, &rtmp, &ff, &mpv)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

async fn player_pause(
    State(state): State<SharedState>,
) -> Result<Json<crate::player::PlayerStatus>, (StatusCode, String)> {
    state
        .player
        .set_paused(true)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

async fn player_resume(
    State(state): State<SharedState>,
) -> Result<Json<crate::player::PlayerStatus>, (StatusCode, String)> {
    state
        .player
        .set_paused(false)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
struct SeekRequest {
    position: f64,
}

async fn player_seek(
    State(state): State<SharedState>,
    Json(req): Json<SeekRequest>,
) -> Result<Json<crate::player::PlayerStatus>, (StatusCode, String)> {
    state
        .player
        .seek(req.position.max(0.0))
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

async fn player_stop(
    State(state): State<SharedState>,
) -> Result<Json<crate::player::PlayerStatus>, (StatusCode, String)> {
    state
        .player
        .stop()
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
struct MonitorRequest {
    monitor: bool,
}

async fn player_monitor(
    State(state): State<SharedState>,
    Json(req): Json<MonitorRequest>,
) -> Result<Json<crate::player::PlayerStatus>, (StatusCode, String)> {
    let (rtmp, ff, mpv) = player_cfg(&state).await;
    state
        .player
        .set_monitor(req.monitor, &rtmp, &ff, &mpv)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}
