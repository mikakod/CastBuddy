//! Built-in file player.
//!
//! Pipeline: mpv (encode mode) -> FLV pacer -> ffmpeg -> local MediaMTX
//! ingest path `player`. The fan-out supervisor and the WebRTC preview pick
//! that up like any other source - no special cases.
//!
//! mpv's encode mode (`--o=-`) internally redirects vo/ao to the encoders
//! (`vo=lavc`/`ao=lavc`, `force-window=no`), so a bare encode instance is
//! *silent and headless by construction* - never pass --vo/--ao flags to it
//! (they override the encoder sinks and break encoding).
//!
//! "Monitor locally" is a second, independent mpv instance playing the same
//! file with normal outputs (window + full-volume audio). Transport commands
//! are broadcast to both; a drift check on each status poll keeps the monitor
//! instance within 2s of the stream.
//!
//! mpv encodes as fast as the CPU allows (not realtime), so the pacer task
//! parses FLV tag timestamps and emits at native speed. It rebases its clock
//! after stalls (pause) and seeks, so pause/resume never fast-forwards.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::state::{with_no_window, SharedState};

/// Local ingest path the player publishes to.
pub const PLAYER_PATH: &str = "player";

const IPC_TIMEOUT: Duration = Duration::from_millis(1500);
/// Lag beyond which the pacer rebases its realtime clock (pause, seek, stall).
const REBASE_BEHIND_MS: i64 = -250;
const REBASE_AHEAD_MS: i64 = 5_000;
/// No paced output for this long while not paused => pusher is wedged.
const HUNG_PACE_MS: i64 = 15_000;
/// Minimum gap between self-heal restarts.
const RESTART_COOLDOWN: Duration = Duration::from_secs(5);
/// How long we let ffmpeg flush after mpv finishes before declaring done.
const FLUSH_GRACE: Duration = Duration::from_secs(10);

fn encoder_ipc() -> String {
    if cfg!(windows) {
        r"\\.\pipe\castbuddy-mpv-enc".to_string()
    } else {
        "/tmp/castbuddy-mpv-enc.sock".to_string()
    }
}

fn monitor_ipc() -> String {
    if cfg!(windows) {
        r"\\.\pipe\castbuddy-mpv-mon".to_string()
    } else {
        "/tmp/castbuddy-mpv-mon.sock".to_string()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlayerStatus {
    pub active: bool,
    pub source: Option<String>,
    pub title: Option<String>,
    pub paused: bool,
    pub time_pos: f64,
    pub duration: Option<f64>,
    /// Local playback audible/visible on this machine.
    pub monitor: bool,
    pub error: Option<String>,
}

#[derive(Default)]
struct Inner {
    encoder: Option<Child>,
    monitor: Option<Child>,
    ffmpeg: Option<Child>,
    /// Last time the pacer moved data; shared with the pacer task.
    heartbeat_ms: Arc<AtomicI64>,
    encoder_dead_since: Option<Instant>,
    last_restart: Option<Instant>,
    status: PlayerStatus,
}

pub struct Player {
    inner: Mutex<Inner>,
}

impl Player {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Start (or restart) playing a local file.
    pub async fn play(
        &self,
        file: &str,
        monitor: bool,
        rtmp_base: &str,
        ffmpeg_bin: &str,
        mpv_bin: &str,
    ) -> Result<PlayerStatus, String> {
        if !Path::new(file).is_file() {
            return Err(format!("file not found: {file}"));
        }
        let mut inner = self.inner.lock().await;
        kill_children(&mut inner);
        inner.status = PlayerStatus {
            source: Some(file.to_string()),
            title: Some(title_of(file)),
            monitor,
            ..Default::default()
        };
        if let Err(e) =
            spawn_pipeline(&mut inner, file, None, false, rtmp_base, ffmpeg_bin, mpv_bin).await
        {
            kill_children(&mut inner);
            inner.status.active = false;
            inner.status.error = Some(e.clone());
            return Err(e);
        }
        inner.status.active = true;
        if monitor {
            if let Err(e) = spawn_monitor(&mut inner, file, 0.0, false, mpv_bin).await {
                tracing::warn!("player: local monitor failed to start: {e}");
                inner.status.monitor = false;
                inner.status.error = Some(format!("monitor unavailable: {e}"));
            }
        }
        tracing::info!("player: playing {file} (monitor={monitor})");
        Ok(inner.status.clone())
    }

    pub async fn status(&self) -> PlayerStatus {
        let mut inner = self.inner.lock().await;
        if inner.status.active && !child_exited(&mut inner.encoder) {
            if let Ok(props) = ipc_query(&encoder_ipc()).await {
                if let Some(v) = props.time_pos {
                    inner.status.time_pos = v;
                }
                if props.duration.is_some() {
                    inner.status.duration = props.duration;
                }
                if props.title.is_some() {
                    inner.status.title = props.title;
                }
                if let Some(p) = props.paused {
                    inner.status.paused = p;
                }
            }
            // Monitor instance: reap if the user closed its window.
            if child_exited(&mut inner.monitor) {
                if let Some(mut c) = inner.monitor.take() {
                    let _ = c.start_kill();
                }
                inner.status.monitor = false;
            }
            // Drift check: keep the local monitor within 2s of the stream.
            if inner.status.monitor && !inner.status.paused {
                if let Some(mon) = ipc_data(&monitor_ipc(), "time-pos").await {
                    if let (Some(mon_pos), stream_pos) = (mon.as_f64(), inner.status.time_pos) {
                        if (mon_pos - stream_pos).abs() > 2.0 {
                            let target = stream_pos.to_string();
                            let _ = ipc_command(
                                &monitor_ipc(),
                                &[
                                    serde_json::json!("seek"),
                                    serde_json::json!(stream_pos),
                                    serde_json::json!("absolute"),
                                ],
                            )
                            .await;
                            tracing::debug!("player: monitor drift {mon_pos:.1} -> {stream_pos:.1} ({target})");
                        }
                    }
                }
            }
        }
        inner.status.clone()
    }

    pub async fn set_paused(&self, paused: bool) -> Result<PlayerStatus, String> {
        let mut inner = self.inner.lock().await;
        require_active(&inner)?;
        ipc_command(
            &encoder_ipc(),
            &[
                serde_json::json!("set_property"),
                serde_json::json!("pause"),
                serde_json::json!(paused),
            ],
        )
        .await?;
        if inner.monitor.is_some() {
            let _ = ipc_command(
                &monitor_ipc(),
                &[
                    serde_json::json!("set_property"),
                    serde_json::json!("pause"),
                    serde_json::json!(paused),
                ],
            )
            .await;
        }
        inner.status.paused = paused;
        Ok(inner.status.clone())
    }

    pub async fn seek(&self, position: f64) -> Result<PlayerStatus, String> {
        let mut inner = self.inner.lock().await;
        require_active(&inner)?;
        ipc_command(
            &encoder_ipc(),
            &[
                serde_json::json!("seek"),
                serde_json::json!(position),
                serde_json::json!("absolute"),
            ],
        )
        .await?;
        if inner.monitor.is_some() {
            let _ = ipc_command(
                &monitor_ipc(),
                &[
                    serde_json::json!("seek"),
                    serde_json::json!(position),
                    serde_json::json!("absolute"),
                ],
            )
            .await;
        }
        inner.status.time_pos = position;
        Ok(inner.status.clone())
    }

    pub async fn stop(&self) -> Result<PlayerStatus, String> {
        let mut inner = self.inner.lock().await;
        kill_children(&mut inner);
        inner.status = PlayerStatus::default();
        tracing::info!("player: stopped");
        Ok(inner.status.clone())
    }

    /// Toggle local monitoring while playing. The stream pipeline is
    /// untouched - the monitor instance is independent, so this is seamless.
    pub async fn set_monitor(
        &self,
        monitor: bool,
        _rtmp_base: &str,
        _ffmpeg_bin: &str,
        mpv_bin: &str,
    ) -> Result<PlayerStatus, String> {
        let mut inner = self.inner.lock().await;
        if inner.status.monitor == monitor {
            return Ok(inner.status.clone());
        }
        let playing = inner.status.active && !child_exited(&mut inner.encoder);
        if !playing {
            inner.status.monitor = monitor;
            return Ok(inner.status.clone());
        }
        if monitor {
            let file = inner
                .status
                .source
                .clone()
                .ok_or_else(|| "no source".to_string())?;
            let pos = ipc_data(&encoder_ipc(), "time-pos")
                .await
                .and_then(|v| v.as_f64())
                .unwrap_or(inner.status.time_pos);
            let paused = ipc_data(&encoder_ipc(), "pause")
                .await
                .and_then(|v| v.as_bool())
                .unwrap_or(inner.status.paused);
            spawn_monitor(&mut inner, &file, pos, paused, mpv_bin).await?;
            inner.status.monitor = true;
            tracing::info!("player: monitor on at {pos:.1}s (paused={paused})");
        } else {
            if let Some(mut c) = inner.monitor.take() {
                let _ = c.start_kill();
            }
            inner.status.monitor = false;
            tracing::info!("player: monitor off");
        }
        Ok(inner.status.clone())
    }

    /// Watchdog tick: reap a finished playback, self-heal a broken pusher.
    pub async fn supervise(&self, rtmp_base: &str, ffmpeg_bin: &str, mpv_bin: &str) {
        let mut inner = self.inner.lock().await;
        if !inner.status.active {
            return;
        }

        // mpv exited: normal end-of-file (let ffmpeg flush) or a crash.
        if child_exited(&mut inner.encoder) {
            let flushed = child_exited(&mut inner.ffmpeg);
            let timed_out = inner
                .encoder_dead_since
                .is_some_and(|t| t.elapsed() > FLUSH_GRACE);
            if flushed || timed_out {
                kill_children(&mut inner);
                inner.status = PlayerStatus::default();
                tracing::info!("player: playback finished");
            } else {
                inner.encoder_dead_since.get_or_insert(Instant::now());
            }
            return;
        }

        // mpv alive: is the pusher healthy?
        let hb_age = now_ms() - inner.heartbeat_ms.load(Ordering::SeqCst);
        let ff_gone = child_exited(&mut inner.ffmpeg);
        if !ff_gone && hb_age < HUNG_PACE_MS {
            inner.encoder_dead_since = None;
            return;
        }

        // Stale heartbeat is legitimate while paused - confirm first.
        let paused = ipc_data(&encoder_ipc(), "pause")
            .await
            .and_then(|v| v.as_bool())
            .unwrap_or(inner.status.paused);
        if paused && !ff_gone {
            return;
        }

        if inner
            .last_restart
            .is_some_and(|t| t.elapsed() < RESTART_COOLDOWN)
        {
            return;
        }
        let Some(file) = inner.status.source.clone() else {
            kill_children(&mut inner);
            inner.status = PlayerStatus::default();
            return;
        };
        let pos = ipc_data(&encoder_ipc(), "time-pos")
            .await
            .and_then(|v| v.as_f64())
            .unwrap_or(inner.status.time_pos);

        tracing::warn!(
            "player: pusher unhealthy (ffmpeg_gone={ff_gone}, heartbeat_age={}s) - restarting at {pos:.1}s",
            hb_age / 1000
        );
        kill_main(&mut inner);
        match spawn_pipeline(&mut inner, &file, Some(pos), paused, rtmp_base, ffmpeg_bin, mpv_bin)
            .await
        {
            Ok(()) => {
                inner.status.active = true;
                inner.status.time_pos = pos;
                inner.status.paused = paused;
                inner.status.error = None;
                inner.last_restart = Some(Instant::now());
            }
            Err(e) => {
                tracing::warn!("player: restart failed: {e}");
                kill_children(&mut inner);
                inner.status = PlayerStatus {
                    error: Some(format!("pipeline restart failed: {e}")),
                    ..Default::default()
                };
            }
        }
    }
}

/// Background watchdog; started from main next to the fan-out supervisor.
pub async fn watchdog(state: SharedState) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let (rtmp, ff, mpv) = {
            let cfg = state.config.read().await;
            (
                cfg.mediamtx.rtmp_url.clone(),
                cfg.server.ffmpeg.clone(),
                cfg.server.mpv.clone(),
            )
        };
        state.player.supervise(&rtmp, &ff, &mpv).await;
    }
}

fn require_active(inner: &Inner) -> Result<(), String> {
    if inner.status.active {
        Ok(())
    } else {
        Err("nothing is playing".into())
    }
}

fn title_of(file: &str) -> String {
    Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.to_string())
}

/// Kill the stream pipeline (encoder + pusher), keep the local monitor.
fn kill_main(inner: &mut Inner) {
    if let Some(mut c) = inner.encoder.take() {
        let _ = c.start_kill();
    }
    if let Some(mut c) = inner.ffmpeg.take() {
        let _ = c.start_kill();
    }
    inner.encoder_dead_since = None;
}

fn kill_children(inner: &mut Inner) {
    kill_main(inner);
    if let Some(mut c) = inner.monitor.take() {
        let _ = c.start_kill();
    }
    inner.last_restart = None;
    inner.encoder_dead_since = None;
}

/// True if the child process has exited (non-blocking); no child = dead.
fn child_exited(child: &mut Option<Child>) -> bool {
    match child {
        Some(c) => matches!(c.try_wait(), Ok(Some(_))),
        None => true,
    }
}

/// Spawn the stream pipeline: encoder mpv -> pacer -> ffmpeg -> MediaMTX.
///
/// `start`/`start_paused` position a (re)spawn mid-file for self-heal.
async fn spawn_pipeline(
    inner: &mut Inner,
    file: &str,
    start: Option<f64>,
    start_paused: bool,
    rtmp_base: &str,
    ffmpeg_bin: &str,
    mpv_bin: &str,
) -> Result<(), String> {
    let target = format!("{}/{}", rtmp_base.trim_end_matches('/'), PLAYER_PATH);
    inner.heartbeat_ms.store(now_ms(), Ordering::SeqCst);

    // --- ffmpeg: remux the paced FLV into the local RTMP ingest ---
    // No -re: the pacer owns realtime pacing.
    let mut ff = Command::new(ffmpeg_bin);
    ff.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "warning",
        "-i",
        "pipe:0",
        "-c",
        "copy",
        "-f",
        "flv",
        "-flvflags",
        "no_duration_filesize",
        &target,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    with_no_window(&mut ff);
    let mut ffmpeg = ff.spawn().map_err(|e| format!("spawning ffmpeg: {e}"))?;
    let ff_stdin = ffmpeg
        .stdin
        .take()
        .ok_or_else(|| "ffmpeg stdin missing".to_string())?;
    if let Some(stderr) = ffmpeg.stderr.take() {
        tokio::spawn(tag_log("ffmpeg", stderr));
    }

    // --- encoder mpv: silent + headless by construction (encode mode). ---
    let ipc = encoder_ipc();
    #[cfg(unix)]
    let _ = std::fs::remove_file(&ipc);
    let mut mp = Command::new(mpv_bin);
    // NOTE: this mpv build only accepts the --option=value form for
    // input-ipc-server; the space form writes "Error parsing commandline
    // option" to stdout (= our FLV pipe) and exits 1.
    //
    // tune=zerolatency: no B-frames, no lookahead, no sync-delay - keeps the
    // stream WebRTC-playable (MediaMTX refuses H264 with B-frames) and cuts
    // encode latency. g=60: 2s keyframe interval at 30fps so HLS segments
    // and preview joins stay fast.
    mp.args([
        "--no-config",
        "--no-terminal",
        "--msg-level=all=warn",
        &format!("--input-ipc-server={ipc}"),
        "--o=-",
        "--of=flv",
        "--ovc=libx264",
        "--ovcopts=preset=veryfast,tune=zerolatency,g=60",
        "--oac=aac",
        "--vf-append=format=yuv420p",
    ]);
    if let Some(pos) = start {
        if pos > 0.2 {
            mp.arg(format!("--start={pos}"));
        }
    }
    if start_paused {
        mp.arg("--pause");
    }
    mp.arg("--").arg(file);
    mp.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    with_no_window(&mut mp);
    let mut encoder = match mp.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut ffmpeg = ffmpeg;
            let _ = ffmpeg.start_kill();
            return Err(format!("spawning mpv: {e}"));
        }
    };
    let mpv_stdout = match encoder.stdout.take() {
        Some(s) => s,
        None => {
            let mut encoder = encoder;
            let _ = encoder.start_kill();
            let mut ffmpeg = ffmpeg;
            let _ = ffmpeg.start_kill();
            return Err("mpv stdout missing".into());
        }
    };
    if let Some(stderr) = encoder.stderr.take() {
        tokio::spawn(tag_log("encoder", stderr));
    }

    let hb = inner.heartbeat_ms.clone();
    tokio::spawn(pacer(mpv_stdout, ff_stdin, hb));

    inner.encoder = Some(encoder);
    inner.ffmpeg = Some(ffmpeg);
    inner.encoder_dead_since = None;
    Ok(())
}

/// Spawn the local monitor instance (normal playback: window + audio),
/// positioned to match the stream.
async fn spawn_monitor(
    inner: &mut Inner,
    file: &str,
    pos: f64,
    paused: bool,
    mpv_bin: &str,
) -> Result<(), String> {
    let ipc = monitor_ipc();
    #[cfg(unix)]
    let _ = std::fs::remove_file(&ipc);
    let mut cmd = Command::new(mpv_bin);
    cmd.args([
        "--no-config",
        "--no-terminal",
        "--msg-level=all=warn",
        &format!("--input-ipc-server={ipc}"),
    ]);
    if pos > 0.2 {
        cmd.arg(format!("--start={pos}"));
    }
    if paused {
        cmd.arg("--pause");
    }
    cmd.arg("--").arg(file);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    with_no_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("spawning mpv: {e}"))?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(tag_log("monitor", stderr));
    }
    inner.monitor = Some(child);
    Ok(())
}

async fn tag_log(tag: &'static str, stream: impl tokio::io::AsyncRead + Unpin) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::warn!(target: "player", "{tag}: {line}");
    }
}

// ---------- FLV realtime pacer ----------
//
// mpv encodes as fast as the CPU allows. We walk FLV tags, derive each tag's
// timestamp, and emit at native speed. The clock rebases when a timestamp is
// far behind (pause/stall/seek-back) or far ahead (seek-forward), so content
// always flows at 1x and never fast-forwards after a pause.

fn pace(epoch: &mut Option<i64>, ts: i64) -> Duration {
    let now = now_ms();
    match *epoch {
        None => {
            *epoch = Some(now - ts);
            Duration::ZERO
        }
        Some(ep) => {
            let delay = (ep + ts) - now;
            if !(REBASE_BEHIND_MS..=REBASE_AHEAD_MS).contains(&delay) {
                *epoch = Some(now - ts);
                Duration::ZERO
            } else if delay > 0 {
                Duration::from_millis(delay as u64)
            } else {
                Duration::ZERO
            }
        }
    }
}

async fn pacer(mut stdout: ChildStdout, mut stdin: ChildStdin, hb: Arc<AtomicI64>) {
    let mut buf: Vec<u8> = Vec::with_capacity(128 * 1024);
    let mut tmp = vec![0u8; 64 * 1024];
    // FLV header: stage 0 = read the fixed 9 bytes (holds dataOffset),
    // stage 1 = read the full header, stage 2 = tags.
    let mut header_stage: u8 = 0;
    let mut header_size: usize = 9;
    let mut epoch: Option<i64> = None;
    let mut total: usize = 0;

    'read: loop {
        let n = match stdout.read(&mut tmp).await {
            Ok(0) | Err(_) => break 'read,
            Ok(n) => n,
        };
        total += n;
        buf.extend_from_slice(&tmp[..n]);

        'drain: loop {
            if header_stage < 2 {
                if header_stage == 0 {
                    if buf.len() < 9 {
                        break 'drain;
                    }
                    if buf.len() >= 3 && &buf[..3] != b"FLV" {
                        let text = String::from_utf8_lossy(&buf[..buf.len().min(200)]);
                        tracing::warn!("pacer: stdout is not FLV, first bytes: {text:?}");
                    }
                    let off = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
                    if (9..=4096).contains(&off) {
                        header_size = off;
                    }
                    header_stage = 1;
                    continue 'drain;
                }
                if buf.len() < header_size {
                    break 'drain;
                }
                // Forward the FLV header itself - without the "FLV" magic the
                // downstream probe rejects the stream as invalid data.
                if stdin.write_all(&buf[..header_size]).await.is_err() {
                    break 'read;
                }
                hb.store(now_ms(), Ordering::SeqCst);
                buf.drain(..header_size);
                header_stage = 2;
                continue 'drain;
            }
            // PreviousTagSize (4) + tag header (11).
            if buf.len() < 15 {
                break 'drain;
            }
            let tag = 4usize;
            let data_size = ((buf[tag + 1] as usize) << 16)
                | ((buf[tag + 2] as usize) << 8)
                | (buf[tag + 3] as usize);
            // FLV timestamp = UI24 big-endian, extended byte on top.
            let ts = ((buf[tag + 4] as i64) << 16)
                | ((buf[tag + 5] as i64) << 8)
                | (buf[tag + 6] as i64)
                | ((buf[tag + 7] as i64) << 24);
            let total = 15 + data_size;
            if buf.len() < total {
                break 'drain;
            }

            let wait = pace(&mut epoch, ts);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            if stdin.write_all(&buf[..total]).await.is_err() {
                break 'read;
            }
            hb.store(now_ms(), Ordering::SeqCst);
            buf.drain(..total);
        }
    }
    let _ = stdin.shutdown().await;
    tracing::info!(target: "player", "pacer exiting ({total} bytes relayed)");
}

// ---------- mpv JSON IPC ----------

struct Query {
    time_pos: Option<f64>,
    duration: Option<f64>,
    title: Option<String>,
    paused: Option<bool>,
}

async fn ipc_query(pipe: &str) -> Result<Query, String> {
    let time_pos = ipc_data(pipe, "time-pos").await.and_then(|v| v.as_f64());
    let duration = ipc_data(pipe, "duration").await.and_then(|v| v.as_f64());
    let title = ipc_data(pipe, "media-title")
        .await
        .and_then(|v| v.as_str().map(String::from));
    let paused = ipc_data(pipe, "pause").await.and_then(|v| v.as_bool());
    Ok(Query {
        time_pos,
        duration,
        title,
        paused,
    })
}

async fn ipc_data(pipe: &str, prop: &str) -> Option<serde_json::Value> {
    let resp = ipc_command(
        pipe,
        &[serde_json::json!("get_property"), serde_json::json!(prop)],
    )
    .await
    .ok()?;
    resp.get("data").cloned()
}

async fn ipc_command(pipe: &str, args: &[serde_json::Value]) -> Result<serde_json::Value, String> {
    let cmd = serde_json::json!({ "command": args });
    match tokio::time::timeout(IPC_TIMEOUT, ipc_roundtrip(pipe, cmd)).await {
        Ok(r) => r,
        Err(_) => Err("mpv IPC timed out".into()),
    }
}

/// mpv creates its IPC endpoint asynchronously right after spawn; retry
/// briefly, but fail fast once it is clear there is no server at all.
#[cfg(windows)]
async fn ipc_connect(pipe: &str) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, String> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut last = None;
    for _ in 0..10 {
        match ClientOptions::new().open(pipe) {
            Ok(c) => return Ok(c),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    Err(format!(
        "mpv IPC pipe not available: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

#[cfg(unix)]
async fn ipc_connect(pipe: &str) -> Result<tokio::net::UnixStream, String> {
    let mut last = None;
    for _ in 0..10 {
        match tokio::net::UnixStream::connect(pipe).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    Err(format!(
        "mpv IPC socket not available: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

async fn ipc_roundtrip(pipe: &str, cmd: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = ipc_connect(pipe).await?;
    write_read(&mut stream, cmd).await
}

async fn write_read<S>(stream: &mut S, cmd: serde_json::Value) -> Result<serde_json::Value, String>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut line = cmd.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("IPC write: {e}"))?;
    let mut reader = BufReader::new(stream);
    // Skip unsolicited events, stop at the first real response.
    for _ in 0..16 {
        let mut resp = String::new();
        let n = reader
            .read_line(&mut resp)
            .await
            .map_err(|e| format!("IPC read: {e}"))?;
        if n == 0 {
            return Err("IPC closed".into());
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(resp.trim()) else {
            continue;
        };
        if val.get("event").is_some() {
            continue;
        }
        let err = val.get("error").and_then(|e| e.as_str()).unwrap_or("");
        if err.is_empty() || err == "success" {
            return Ok(val);
        }
        return Err(format!("mpv: {err}"));
    }
    Err("IPC: too many events, no response".into())
}
