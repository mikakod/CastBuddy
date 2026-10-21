mod access;
mod api;
mod config;
mod fanout;
mod keys;
mod mediamtx;
mod player;
mod share;
mod state;

use std::path::PathBuf;
use std::time::Duration;

use crate::config::{Config, MediaMtxMode};
use crate::state::SharedState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("castbuddy.toml"));

    let cfg = Config::load_or_create(&config_path)?;

    // The guard must outlive the subscriber or the file writer dies silently.
    // The access-logging preference is baked into the filter at startup.
    let _log_guard = init_logging(cfg.server.enable_logging);

    let state = state::AppState::new(cfg.clone(), config_path);

    if cfg.mediamtx.mode == MediaMtxMode::Managed {
        if let Err(e) = state.start_mediamtx().await {
            tracing::warn!("could not start managed mediamtx (is the binary on PATH?): {e:#}");
        }
    }

    tokio::spawn(fanout::supervisor(state.clone()));
    tokio::spawn(player::watchdog(state.clone()));
    tokio::spawn(mediamtx::monitor(state.clone()));
    tokio::spawn(mediamtx_exit_watch(state.clone()));

    // Public share listener (watch pages + file downloads), separate from
    // the 127.0.0.1 dashboard. Bind stays local unless changed on purpose.
    // The public listener is the only one that ever faces strangers, so it
    // gets connect-info (IP logging), rate limiting and security headers.
    let share_app = {
        let public_addr = format!("{}:{}", cfg.server.public_bind, cfg.server.public_port);
        match tokio::net::TcpListener::bind(&public_addr).await {
            Ok(l) => {
                tracing::info!("share server on http://{public_addr}");
                Some(l)
            }
            Err(e) => {
                tracing::error!("could not bind share server on {public_addr}: {e}");
                None
            }
        }
    };

    let bind = format!("{}:{}", cfg.server.bind, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("dashboard on http://{bind}");

    let dashboard = api::router(state.clone(), cfg.server.web_dist.clone(), cfg.server.port)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();

    let dashboard_serve =
        axum::serve(listener, dashboard).with_graceful_shutdown(shutdown_signal("dashboard"));

    match share_app {
        Some(l) => {
            let share = share::router(state.clone())
                .into_make_service_with_connect_info::<std::net::SocketAddr>();
            let share_serve =
                axum::serve(l, share).with_graceful_shutdown(shutdown_signal("share server"));
            let _ = tokio::join!(dashboard_serve, share_serve);
        }
        None => dashboard_serve.await?,
    }

    state.stop_mediamtx().await;
    tracing::info!("bye");
    Ok(())
}

/// Stdout + a daily-rolling file under `logs/`, so troubleshooting survives
/// a closed terminal and rotation keeps the directory bounded over time.
///
/// `enable_access_logging = false` silences the `access` target and
/// MediaMTX's console passthrough - the only two places a visitor's IP can
/// end up in a record (GDPR: record nothing instead of serving a notice).
/// Lifecycle logs keep flowing, and enforcement (rate limits, key checks)
/// is unaffected: it never depended on logging.
fn init_logging(enable_access_logging: bool) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::fmt::writer::MakeWriterExt;

    let file = tracing_appender::rolling::daily("logs", "castbuddy.log");
    let (file, file_guard) = tracing_appender::non_blocking(file);
    // RUST_LOG (if set) is the base, but enable-logging has the last word:
    // when off, access records stay suppressed no matter what.
    let base = std::env::var("RUST_LOG").unwrap_or_else(|_| "castbuddy=info,info".to_string());
    let filter = tracing_subscriber::EnvFilter::try_new(format!(
        "{base}{}",
        access_logging_directives(enable_access_logging)
    ))
    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("castbuddy=info,info"));
    // Plain text in the log file: greppable with Select-String / grep.
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(std::io::stdout.and(file))
        .init();
    file_guard
}

/// EnvFilter add-ons when the operator turned access logging off. Empty
/// keeps the default filter untouched.
fn access_logging_directives(enabled: bool) -> &'static str {
    if enabled {
        ""
    } else {
        ",access=off,mediamtx=off"
    }
}

/// Loud log line if the managed MediaMTX child dies on its own (the status
/// page shows "unreachable" anyway, but the log should say why).
async fn mediamtx_exit_watch(state: SharedState) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let mut guard = state.managed.lock().await;
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::error!(
                        "managed mediamtx exited unexpectedly ({status}) - streams and previews are down; restart it from the dashboard"
                    );
                    *guard = None;
                }
                Ok(None) => {}
                Err(e) => tracing::error!("waiting on mediamtx failed: {e}"),
            }
        }
    }
}

async fn shutdown_signal(listener: &'static str) {
    #[cfg(unix)]
    {
        let ctrl_c = async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        };
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    tracing::info!("shutdown signal received ({listener})");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_logging_directives_build_valid_filters() {
        let on = format!("castbuddy=info,info{}", access_logging_directives(true));
        let off = format!("castbuddy=info,info{}", access_logging_directives(false));
        assert!(tracing_subscriber::EnvFilter::try_new(&on).is_ok());
        assert!(tracing_subscriber::EnvFilter::try_new(&off).is_ok());
        // Enabled adds nothing to the default filter at all.
        assert!(access_logging_directives(true).is_empty());
        assert!(!access_logging_directives(false).is_empty());
    }
}
