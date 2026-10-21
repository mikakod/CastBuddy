use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::state::SharedState;

/// Thin client for the MediaMTX control API (/v3).
pub struct MediaMtxClient {
    api_base: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathInfo {
    pub name: String,
    #[serde(default)]
    pub ready: bool,
    /// The connected source (this MediaMTX version reports the publisher here).
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub bytes_received: u64,
    #[allow(dead_code)] // mirrors MediaMTX API; useful in status later
    #[serde(default)]
    pub bytes_sent: u64,
    /// Playback readers; MediaMTX reports an array of reader objects.
    #[serde(default)]
    pub readers: Option<serde_json::Value>,
    /// Older MediaMTX versions report the publisher as its own object.
    #[serde(default)]
    pub publisher: Option<Publisher>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    /// Connection id (present while a publisher is attached).
    #[serde(default)]
    pub id: Option<String>,
    /// Publisher address as "ip:port" (schema varies between versions).
    #[serde(default)]
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Publisher {
    #[allow(dead_code)] // mirrors MediaMTX API; useful in status later
    #[serde(default)]
    pub id: Option<String>,
    /// Publisher address as "ip:port" (schema varies between versions).
    #[serde(default)]
    pub ip: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathsList {
    #[serde(default)]
    items: Vec<PathInfo>,
}

impl PathInfo {
    /// True while a publisher is attached AND the path is ready to be read
    /// (tracks negotiated). Handles both schema variants: newer MediaMTX
    /// reports `source.id`, older ones a `publisher` object.
    pub fn is_publishing(&self) -> bool {
        let has_source = self.publisher.is_some()
            || self
                .source
                .as_ref()
                .and_then(|s| s.id.as_deref())
                .is_some_and(|id| !id.is_empty());
        has_source && self.ready
    }

    pub fn viewer_count(&self) -> Option<usize> {
        match &self.readers {
            Some(serde_json::Value::Array(a)) => Some(a.len()),
            Some(serde_json::Value::Number(n)) => n.as_u64().map(|v| v as usize),
            _ => None,
        }
    }

    /// Publisher address, whichever schema variant reported it. Strips the
    /// port if present ("10.0.0.5:52341" -> "10.0.0.5").
    pub fn publisher_ip(&self) -> Option<String> {
        let raw = self
            .publisher
            .as_ref()
            .and_then(|p| p.ip.clone())
            .or_else(|| self.source.as_ref().and_then(|s| s.ip.clone()))?;
        let host = raw.rsplit_once(':').map_or(raw.as_str(), |(h, _)| h);
        if host.is_empty() {
            None
        } else {
            Some(host.to_string())
        }
    }
}

impl MediaMtxClient {
    pub fn new(api_base: String) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
        }
    }

    pub async fn paths(&self, http: &reqwest::Client) -> anyhow::Result<Vec<PathInfo>> {
        let url = format!("{}/v3/paths/list", self.api_base);
        let list: PathsList = http.get(&url).send().await?.error_for_status()?.json().await?;
        Ok(list.items)
    }

    /// The path currently receiving a publish, if any. When several are
    /// live we pick the one with the highest byte counter, a good proxy
    /// for "the actively streaming one".
    pub async fn active_ingest(&self, http: &reqwest::Client) -> anyhow::Result<Option<PathInfo>> {
        let items = self.paths(http).await?;
        Ok(items
            .into_iter()
            .filter(|p| p.is_publishing())
            .max_by_key(|p| p.bytes_received))
    }

    /// Replace the internal user database at runtime - no restart, connected
    /// publishers stay up. Returns Err only if the API rejects the patch.
    pub async fn patch_auth(
        &self,
        http: &reqwest::Client,
        users: Vec<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v3/config/global/patch", self.api_base);
        let body = serde_json::json!({ "authInternalUsers": users });
        http.patch(&url).json(&body).send().await?.error_for_status()?;
        Ok(())
    }

    /// Remote address of a live RTMP connection (matched by connection id,
    /// which the paths API reports as `source.id`), host part only.
    pub async fn connection_ip(&self, http: &reqwest::Client, conn_id: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct Conns {
            #[serde(default)]
            items: Vec<Conn>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Conn {
            id: String,
            #[serde(default)]
            remote_addr: Option<String>,
        }
        let url = format!("{}/v3/rtmpconns/list", self.api_base);
        let list: Conns = http.get(&url).send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
        let conn = list.items.into_iter().find(|c| c.id == conn_id)?;
        conn.remote_addr.map(|ra| {
            ra.rsplit_once(':')
                .map_or(ra.clone(), |(host, _)| host.to_string())
        })
    }
}

// ---------- stream session monitor ----------
//
// Polls the control API and logs publish transitions only (never steady
// state), so the log answers "who streamed, from which IP, when did it
// stop" without growing while a stream runs.

const MONITOR_TICK: Duration = Duration::from_secs(2);

pub async fn monitor(state: SharedState) {
    let mut tick = tokio::time::interval(MONITOR_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // path -> (first seen, publisher ip)
    let mut live: HashMap<String, (Instant, Option<String>)> = HashMap::new();
    let mut api_down = false;
    loop {
        tick.tick().await;
        let paths = match state.mediamtx.paths(&state.http).await {
            Ok(p) => p,
            Err(e) => {
                if !api_down {
                    api_down = true;
                    tracing::warn!("stream monitor: mediamtx api unreachable: {e:#}");
                }
                continue;
            }
        };
        if api_down {
            api_down = false;
            tracing::info!("stream monitor: mediamtx api reachable again");
        }

        // name -> (direct ip if the schema exposed one, connection id)
        let publishing: HashMap<String, (Option<String>, Option<String>)> = paths
            .iter()
            .filter(|p| p.is_publishing())
            .map(|p| {
                let conn = p
                    .source
                    .as_ref()
                    .and_then(|s| s.id.clone())
                    .or_else(|| p.publisher.as_ref().and_then(|q| q.id.clone()));
                (p.name.clone(), (p.publisher_ip(), conn))
            })
            .collect();

        for (name, (direct_ip, conn)) in &publishing {
            if !live.contains_key(name) {
                // Resolve the publisher's IP: some schema versions expose it
                // on the path itself, v1.11 makes us ask the connection list.
                let ip = match direct_ip {
                    Some(ip) => Some(ip.clone()),
                    None => match conn {
                        Some(id) => state.mediamtx.connection_ip(&state.http, id).await,
                        None => None,
                    },
                };
                tracing::info!(
                    target: "access",
                    "stream started: path={name} publisher={}",
                    ip.as_deref().unwrap_or("unknown")
                );
                live.insert(name.clone(), (Instant::now(), ip));
            }
        }
        let ended: Vec<String> = live
            .keys()
            .filter(|n| !publishing.contains_key(*n))
            .cloned()
            .collect();
        for name in ended {
            if let Some((at, _ip)) = live.remove(&name) {
                tracing::info!(
                    target: "access",
                    "stream ended: path={name} duration={}", human_secs(at.elapsed().as_secs())
                );
            }
        }
    }
}

fn human_secs(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}
