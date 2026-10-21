//! Fan-out supervisor: watches MediaMTX for an active publish, then keeps
//! one `ffmpeg -c copy` process per enabled destination pushing to it.
//! Restart with exponential backoff; a stable run resets the backoff.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::config::Destination;
use crate::mediamtx::MediaMtxClient;
use crate::state::{with_no_window, SharedState};

const LOG_LINES: usize = 200;
const BASE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A run at least this long is considered stable -> reset backoff.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// How often we poll MediaMTX and reap children.
const TICK: Duration = Duration::from_secs(2);

pub type LogBuffer = Arc<Mutex<VecDeque<String>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Running,
    Backoff,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Stopped => "stopped",
            Status::Running => "running",
            Status::Backoff => "backoff",
        }
    }
}

pub struct DestState {
    pub status: Status,
    pub child: Option<Child>,
    pub ingest_path: Option<String>,
    pub started_at: Option<Instant>,
    pub retry_at: Option<Instant>,
    pub backoff: Duration,
    pub last_error: Option<String>,
    pub logs: LogBuffer,
}

impl Default for DestState {
    fn default() -> Self {
        Self {
            status: Status::Stopped,
            child: None,
            ingest_path: None,
            started_at: None,
            retry_at: None,
            backoff: BASE_BACKOFF,
            last_error: None,
            logs: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

async fn push_log(logs: &LogBuffer, line: String) {
    let mut l = logs.lock().await;
    if l.len() >= LOG_LINES {
        l.pop_front();
    }
    l.push_back(line);
}

pub async fn supervisor(state: SharedState) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = tick_once(&state).await {
            tracing::warn!("fanout tick failed: {e:#}");
        }
    }
}

async fn tick_once(state: &SharedState) -> anyhow::Result<()> {
    let (destinations, api_url, rtmp_base, ffmpeg_bin) = {
        let cfg = state.config.read().await;
        (
            cfg.destinations.clone(),
            cfg.mediamtx.api_url.clone(),
            cfg.mediamtx.rtmp_url.clone(),
            cfg.server.ffmpeg.clone(),
        )
    };

    // If MediaMTX's API is unreachable we leave everything running:
    // the local pull will fail on its own if the ingest really died.
    let client = MediaMtxClient::new(api_url);
    let active = match client.active_ingest(&state.http).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("mediamtx api unreachable, skipping tick: {e:#}");
            return Ok(());
        }
    };

    let mut map = state.fanout.lock().await;

    // Drop states for destinations that were deleted from the config.
    let ids: Vec<String> = destinations.iter().map(|d| d.id.clone()).collect();
    let stale: Vec<String> = map.keys().filter(|k| !ids.contains(k)).cloned().collect();
    for id in stale {
        if let Some(mut st) = map.remove(&id) {
            if let Some(mut child) = st.child.take() {
                let _ = child.start_kill();
            }
        }
    }

    for dest in &destinations {
        let st = map.entry(dest.id.clone()).or_default();

        // Reap a child that exited on its own.
        if let Some(mut child) = st.child.take() {
            match child.try_wait() {
                Ok(None) => {
                    // Still running; put it back.
                    st.child = Some(child);
                }
                Ok(Some(exit)) => {
                    let ran = st.started_at.map(|t| t.elapsed()).unwrap_or_default();
                    if ran >= STABLE_AFTER {
                        st.backoff = BASE_BACKOFF;
                    }
                    let wait = st.backoff;
                    st.backoff = (st.backoff * 2).min(MAX_BACKOFF);
                    let msg = format!("ffmpeg exited ({exit}), retrying in {wait:?}");
                    tracing::warn!("{}: {msg}", dest.name);
                    push_log(&st.logs, msg).await;
                    st.last_error = Some(format!("ffmpeg exited: {exit}"));
                    st.ingest_path = None;
                    st.started_at = None;
                    st.status = Status::Backoff;
                    st.retry_at = Some(Instant::now() + wait);
                }
                Err(e) => {
                    push_log(&st.logs, format!("wait on ffmpeg failed: {e}")).await;
                    st.last_error = Some(e.to_string());
                    st.status = Status::Backoff;
                    st.retry_at = Some(Instant::now() + st.backoff);
                }
            }
        }

        let desired = if dest.enabled {
            active.as_ref().map(|p| p.name.clone())
        } else {
            None
        };

        match (st.child.is_some(), desired) {
            // Running: stop when ingest is gone, restart when the path changed.
            (true, Some(path)) => {
                if st.ingest_path.as_deref() != Some(path.as_str()) {
                    if let Some(mut child) = st.child.take() {
                        let _ = child.start_kill();
                    }
                    push_log(&st.logs, format!("ingest path changed to `{path}`, re-attaching")).await;
                    st.status = Status::Backoff;
                    st.retry_at = Some(Instant::now());
                }
            }
            (true, None) => {
                if let Some(mut child) = st.child.take() {
                    let _ = child.start_kill();
                }
                push_log(&st.logs, "ingest stopped, stopping fan-out".to_string()).await;
                st.status = Status::Stopped;
                st.ingest_path = None;
                st.started_at = None;
                st.retry_at = None;
                st.last_error = None;
            }
            // Stopped/backoff: start when there is something to pull.
            (false, Some(path)) => {
                let due = st.retry_at.map_or(true, |t| Instant::now() >= t);
                if due {
                    spawn(st, dest, &rtmp_base, &path, &ffmpeg_bin).await;
                }
            }
            (false, None) => {
                if st.status != Status::Backoff {
                    st.status = Status::Stopped;
                }
            }
        }
    }
    Ok(())
}

async fn spawn(
    st: &mut DestState,
    dest: &Destination,
    rtmp_base: &str,
    path: &str,
    ffmpeg_bin: &str,
) {
    let input = format!("{}/{}", rtmp_base.trim_end_matches('/'), path);
    let output = dest.target_url();
    let output_host = dest.url.clone();

    let mut cmd = Command::new(ffmpeg_bin);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "warning",
        "-i",
        &input,
        "-c",
        "copy",
        "-f",
        "flv",
        "-flvflags",
        "no_duration_filesize",
        &output,
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    with_no_window(&mut cmd);

    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(stderr) = child.stderr.take() {
                let logs = st.logs.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, BufReader};
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        push_log(&logs, line).await;
                    }
                });
            }
            let msg = format!("pushing `{input}` -> {output_host}");
            tracing::info!("{}: {msg}", dest.name);
            push_log(&st.logs, msg).await;
            st.child = Some(child);
            st.status = Status::Running;
            st.ingest_path = Some(path.to_string());
            st.started_at = Some(Instant::now());
            st.retry_at = None;
            st.last_error = None;
        }
        Err(e) => {
            let msg = format!("failed to spawn ffmpeg: {e}");
            tracing::error!("{}: {msg}", dest.name);
            push_log(&st.logs, msg.clone()).await;
            st.last_error = Some(msg);
            st.status = Status::Backoff;
            st.retry_at = Some(Instant::now() + st.backoff);
            st.backoff = (st.backoff * 2).min(MAX_BACKOFF);
        }
    }
}
