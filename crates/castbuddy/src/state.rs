use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, RwLock};

use crate::access::{AccessLog, RateLimiter, SHARE_BURST, SHARE_RATE_PER_SEC};
use crate::config::{Config, MediaMtxMode};
use crate::fanout::DestState;
use crate::mediamtx::MediaMtxClient;
use crate::player::Player;

pub struct AppState {
    pub config: RwLock<Config>,
    pub config_path: PathBuf,
    pub http: reqwest::Client,
    pub mediamtx: MediaMtxClient,
    pub managed: Mutex<Option<tokio::process::Child>>,
    pub fanout: Mutex<HashMap<String, DestState>>,
    pub player: Player,
    /// Session dedup for the public access log.
    pub access: AccessLog,
    /// Per-IP token bucket for the public share listener.
    pub rate: RateLimiter,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> SharedState {
        let mediamtx = MediaMtxClient::new(config.mediamtx.api_url.clone());
        // Every upstream fetch is loopback (MediaMTX) or a local API call, so
        // short timeouts are safe - and they stop a wedged upstream from
        // pinning connections forever.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(32)
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent(concat!("CastBuddy/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Arc::new(Self {
            config: RwLock::new(config),
            config_path,
            http,
            mediamtx,
            managed: Mutex::new(None),
            fanout: Mutex::new(HashMap::new()),
            player: Player::new(),
            access: AccessLog::new(),
            rate: RateLimiter::new(SHARE_RATE_PER_SEC, SHARE_BURST),
        })
    }

    /// Spawn MediaMTX with a generated config (managed mode).
    pub async fn start_mediamtx(&self) -> anyhow::Result<()> {
        let (binary, cfg_path, mtx) = {
            let cfg = self.config.read().await;
            (
                cfg.mediamtx.binary.clone(),
                cfg.mediamtx.config_path.clone(),
                cfg.clone(),
            )
        };
        let cfg_path = std::path::PathBuf::from(cfg_path);
        std::fs::write(&cfg_path, crate::config::generated_mediamtx_config(&mtx))?;

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.arg(&cfg_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        with_no_window(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning `{binary}`: {e}"))?;

        // MediaMTX logs to stdout; capture both streams.
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(log_pipe("mediamtx", stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(log_pipe("mediamtx", stderr));
        }

        *self.managed.lock().await = Some(child);
        tracing::info!("managed mediamtx started ({binary} {})", cfg_path.display());
        Ok(())
    }

    pub async fn stop_mediamtx(&self) {
        if let Some(mut child) = self.managed.lock().await.take() {
            let _ = child.start_kill();
            tracing::info!("managed mediamtx stopped");
        }
    }

    pub async fn mode(&self) -> MediaMtxMode {
        self.config.read().await.mediamtx.mode
    }
}

async fn log_pipe(tag: &'static str, stream: impl tokio::io::AsyncRead + Unpin) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(target: "mediamtx", "{tag}: {line}");
    }
}

/// Spawn without flashing a console window on Windows.
pub fn with_no_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        // tokio::process::Command has its own inherent creation_flags method.
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = cmd;
}
