use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub mediamtx: MediaMtxConfig,
    #[serde(default)]
    pub destinations: Vec<Destination>,
    /// Publish (stream) keys: external publishers (OBS etc.) with a key may
    /// push to their dedicated MediaMTX path from anywhere.
    #[serde(default)]
    pub publish_keys: Vec<PublishKey>,
    /// Share links (live watch pages, file/folder downloads) served on the
    /// public listener.
    #[serde(default)]
    pub shares: Vec<Share>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address for the dashboard/API. Keep on 127.0.0.1 unless you
    /// know what you're doing (no auth on the API yet).
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Built dashboard assets (web/dist).
    #[serde(default = "default_web_dist")]
    pub web_dist: String,
    #[serde(default = "default_ffmpeg")]
    pub ffmpeg: String,
    #[serde(default = "default_mpv")]
    pub mpv: String,
    /// Root directory the dashboard's file browser is confined to.
    /// Relative paths in play requests resolve against this too.
    #[serde(default = "default_media_dir")]
    pub media_dir: String,
    /// Public listener for share links (live watch pages + file downloads).
    /// Separate from the dashboard so the admin API can stay on 127.0.0.1
    /// while shares are reachable from your LAN / the internet.
    #[serde(default = "default_public_bind")]
    pub public_bind: String,
    #[serde(default = "default_public_port")]
    pub public_port: u16,
    /// Rewrite share links shown in the dashboard with this base URL instead
    /// of the hostname you browse the dashboard from. For reverse-proxy
    /// setups (Caddy, Tailscale Serve): "example.com",
    /// "https://example.com", "https://example.com:8443", ...
    /// Scheme defaults to http:// when omitted.
    #[serde(default, rename = "public-url", alias = "public_url")]
    pub public_url: String,
    /// Whether `public_url` is used for generated links. Off = links derive
    /// from the dashboard's hostname and `public_port` (previous behavior).
    /// Accepts `true`/`false` and `yes`/`no`.
    #[serde(
        default,
        rename = "public-url-enabled",
        alias = "public_url_enabled",
        deserialize_with = "deserialize_bool_ish"
    )]
    pub public_url_enabled: bool,
    /// Write the access log: request lines with client IPs, share
    /// allow/deny, stream start/end, MediaMTX console output. Set `false`
    /// to record nothing about who connected (GDPR: no viewer IPs anywhere,
    /// so no notice is needed on that account). Rate limiting and every
    /// access rule stay fully active either way - only the records are
    /// gone. Accepts `true`/`false` and `yes`/`no`; applied at startup.
    #[serde(
        default = "default_true",
        rename = "enable-logging",
        alias = "enable_logging",
        deserialize_with = "deserialize_bool_ish"
    )]
    pub enable_logging: bool,
}

/// Booleans as `true`/`false` (TOML) or `yes`/`no`/`on`/`off` (strings) -
/// the config docs promise `public-url-enabled = yes`, so honor it.
fn deserialize_bool_ish<'de, D>(d: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoolIsh;
    impl<'de> serde::de::Visitor<'de> for BoolIsh {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a boolean: true/false or yes/no")
        }
        fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<bool, E> {
            match v.trim().to_ascii_lowercase().as_str() {
                "yes" | "true" | "on" => Ok(true),
                "no" | "false" | "off" => Ok(false),
                other => Err(E::custom(format!(
                    "expected yes/no (or true/false), got {other:?}"
                ))),
            }
        }
    }
    d.deserialize_any(BoolIsh)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaMtxConfig {
    /// "external": CastBuddy talks to an already-running MediaMTX (e.g. Docker).
    /// "managed":  CastBuddy spawns the mediamtx binary itself with a generated config.
    #[serde(default)]
    pub mode: MediaMtxMode,
    #[serde(default = "default_api_url")]
    /// API URL CastBuddy calls. In managed mode the generated config binds
    /// MediaMTX's API to this URL's host:port, so overriding it stays
    /// consistent (default: http://127.0.0.1:9997).
    pub api_url: String,
    #[serde(default = "default_rtmp_url")]
    pub rtmp_url: String,
    #[serde(default = "default_webrtc_url")]
    pub webrtc_url: String,
    #[serde(default = "default_hls_url")]
    pub hls_url: String,
    /// Only used in managed mode.
    #[serde(default = "default_mediamtx_binary")]
    pub binary: String,
    #[serde(default = "default_mediamtx_config_path")]
    pub config_path: String,
    /// PEM certificate/key enabling RTMPS (rtmps:// :1936). Both must be set.
    /// RTMP (:1935) keeps working alongside ("optional" encryption).
    #[serde(default)]
    pub tls_cert: String,
    #[serde(default)]
    pub tls_key: String,
    /// Bind host for the RTMPS listener (:1936; only used when tls_cert +
    /// tls_key are set). Empty = same host as `rtmp_bind`. Set "0.0.0.0" to
    /// accept guest publishers over the internet; cleartext :1935 is then
    /// pinned to loopback, so non-localhost ingests MUST be RTMPS.
    #[serde(default)]
    pub rtmps_bind: String,
    /// Strict publishing: only `ingest`, `player` and stream-key paths exist;
    /// unknown path names are refused (no wildcard publishing).
    #[serde(default)]
    pub strict: bool,
    /// Bind host for the cleartext RTMP listener (managed mode). Loopback by
    /// default so nothing is LAN/internet reachable unless you opt in. Note:
    /// when the RTMPS listener is exposed (`rtmps_bind` non-loopback) this is
    /// forced to loopback anyway - unencrypted ingests never leave the box.
    #[serde(default = "default_loopback")]
    pub rtmp_bind: String,
    /// Bind host for the HLS listener (managed mode). Loopback by default;
    /// the dashboard preview and share proxy reach it locally.
    #[serde(default = "default_loopback")]
    pub hls_bind: String,
    /// Bind host for the WebRTC HTTP listener *and* its ICE/UDP mux (:8189,
    /// `webrtcLocalUDPAddress`) in managed mode. Loopback by default - the
    /// dashboard preview only ever runs on the same machine (the dashboard
    /// itself is loopback-only), so loopback ICE is sufficient.
    #[serde(default = "default_loopback")]
    pub webrtc_bind: String,
    /// Hostname shown in generated publisher URLs. Empty = derive from rtmp_url.
    #[serde(default)]
    pub public_host: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MediaMtxMode {
    External,
    /// The default: the daemon spawns and supervises MediaMTX itself, so a
    /// fresh `castbuddy.toml` (quick start: `cargo run`) streams out of the
    /// box. External mode is an explicit opt-in (Docker / pre-running MTX).
    #[default]
    Managed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// Base RTMP URL, e.g. rtmp://a.rtmp.youtube.com/live2
    pub url: String,
    /// Stream key appended to the URL. Leave empty if the key is already in the URL.
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub enabled: bool,
}

impl Destination {
    /// URL + key joined the way RTMP services expect.
    pub fn target_url(&self) -> String {
        let base = self.url.trim_end_matches('/');
        if self.key.trim().is_empty() {
            base.to_string()
        } else {
            format!("{}/{}", base, self.key.trim_start_matches('/'))
        }
    }
}

/// A credential letting an external publisher (OBS, ffmpeg, ...) push to a
/// dedicated MediaMTX path from anywhere. `key` is 256 bits of CSPRNG
/// entropy; MediaMTX only ever sees its SHA-256 hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishKey {
    /// Slug; doubles as the RTMP username (shows up in mediamtx logs).
    pub id: String,
    pub name: String,
    /// MediaMTX path this key may publish to.
    pub path: String,
    /// The secret, 52-char base32. Shown in the dashboard (local-only API).
    pub key: String,
    /// Unix seconds.
    pub created: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_loopback() -> String {
    "127.0.0.1".to_string()
}

fn default_public_bind() -> String {
    "127.0.0.1".into()
}

fn default_public_port() -> u16 {
    49989
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An unguessable share link: a live watch page or a file/folder download,
/// served on the public listener. The `id` is the secret URL segment -
/// treat a link like a password.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    /// 128 bits of CSPRNG entropy, Crockford base32 - the secret URL part.
    pub id: String,
    pub name: String,
    /// "live" = watch page for a MediaMTX path; "file"/"folder" = downloads
    /// under `server.media_dir`.
    pub kind: String,
    /// MediaMTX path (live) or media_dir-relative path (file/folder).
    pub target: String,
    /// Unix seconds; 0 = never expires.
    #[serde(default)]
    pub expires: u64,
    /// Unix seconds.
    pub created: u64,
}

impl Share {
    pub fn is_expired(&self) -> bool {
        self.expires != 0 && self.expires <= unix_now()
    }

    /// URL path this share is served under (on the public listener).
    pub fn url_path(&self) -> String {
        match self.kind.as_str() {
            "live" => format!("/watch/{}", self.id),
            _ => format!("/d/{}", self.id),
        }
    }
}

impl MediaMtxConfig {
    pub fn tls_enabled(&self) -> bool {
        !self.tls_cert.trim().is_empty() && !self.tls_key.trim().is_empty()
    }

    /// Effective RTMPS bind host; falls back to `rtmp_bind` when unset.
    pub fn rtmps_bind(&self) -> &str {
        let s = self.rtmps_bind.trim();
        if s.is_empty() {
            self.rtmp_bind.trim()
        } else {
            s
        }
    }

    fn is_loopback(host: &str) -> bool {
        matches!(host, "127.0.0.1" | "localhost" | "::1")
    }

    /// True when RTMPS would listen on a non-loopback address - i.e. remote
    /// publishers are invited. Cleartext :1935 is then pinned to loopback so
    /// no unencrypted ingest can leave this machine.
    pub fn rtmps_public(&self) -> bool {
        self.tls_enabled() && !Self::is_loopback(self.rtmps_bind())
    }

    /// Cleartext ingest URL for software running on this machine (the
    /// dashboard's own OBS/VLC hints). When RTMPS is exposed the cleartext
    /// port is loopback-only, so that is the address that always works here.
    pub fn local_ingest_url(&self) -> String {
        if self.rtmps_public() {
            "rtmp://127.0.0.1:1935".into()
        } else {
            self.rtmp_url.trim().trim_end_matches('/').to_string()
        }
    }

    /// Base URL publishers should use (`rtmp[s]://host:port`), honoring TLS
    /// and the configured public hostname. `fallback_host` (e.g. the hostname
    /// of `public-url`) is used when `public_host` is empty, before falling
    /// back to the host embedded in `rtmp_url`.
    pub fn public_rtmp_url(&self, fallback_host: Option<&str>) -> String {
        let host = if self.public_host.trim().is_empty() {
            fallback_host
                .map(str::to_string)
                .unwrap_or_else(|| {
                    self.rtmp_url
                        .trim()
                        .trim_start_matches("rtmp://")
                        .trim_start_matches("rtmps://")
                        .split('/')
                        .next()
                        .unwrap_or("127.0.0.1")
                        .split(':')
                        .next()
                        .unwrap_or("127.0.0.1")
                        .to_string()
                })
        } else {
            self.public_host.trim().to_string()
        };
        if self.tls_enabled() {
            format!("rtmps://{host}:1936")
        } else {
            format!("rtmp://{host}:1935")
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8080
}
fn default_web_dist() -> String {
    "web/dist".into()
}
fn default_ffmpeg() -> String {
    "ffmpeg".into()
}
fn default_mpv() -> String {
    "mpv".into()
}
fn default_media_dir() -> String {
    ".".into()
}
fn default_api_url() -> String {
    "http://127.0.0.1:9997".into()
}
fn default_rtmp_url() -> String {
    "rtmp://127.0.0.1:1935".into()
}
fn default_webrtc_url() -> String {
    "http://127.0.0.1:8889".into()
}
fn default_hls_url() -> String {
    "http://127.0.0.1:8888".into()
}
fn default_mediamtx_binary() -> String {
    "mediamtx".into()
}
fn default_mediamtx_config_path() -> String {
    "mediamtx.generated.yml".into()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            web_dist: default_web_dist(),
            ffmpeg: default_ffmpeg(),
            mpv: default_mpv(),
            media_dir: default_media_dir(),
            public_bind: default_public_bind(),
            public_port: default_public_port(),
            public_url: String::new(),
            public_url_enabled: false,
            enable_logging: true,
        }
    }
}

impl ServerConfig {
    /// Resolved base URL for share links generated in the dashboard, e.g.
    /// `http://example.com` when `public-url-enabled = true` and
    /// `public-url = "example.com"`. Returns `None` when the override
    /// is off or empty - callers then fall back to the dashboard's own
    /// hostname plus `public_port` (previous behavior).
    ///
    /// A scheme is optional in the config value; bare hostnames get `http://`
    /// (the usual reverse-proxy-on-80/443 setup). Include `:port` yourself
    /// for anything non-standard.
    pub fn public_base_url(&self) -> Option<String> {
        if !self.public_url_enabled {
            return None;
        }
        let raw = self.public_url.trim().trim_end_matches('/');
        if raw.is_empty() {
            return None;
        }
        if raw.contains("://") {
            Some(raw.to_string())
        } else {
            Some(format!("http://{raw}"))
        }
    }

    /// Hostname inside `public_url` (scheme, port and path stripped), if
    /// configured. Used as the default host for generated publisher URLs so
    /// one `public-url` setting drives both share links and stream keys.
    pub fn public_url_hostname(&self) -> Option<String> {
        let base = self.public_base_url()?;
        let host = base
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        (!host.is_empty()).then(|| host.to_string())
    }
}

impl Default for MediaMtxConfig {
    fn default() -> Self {
        Self {
            mode: MediaMtxMode::default(),
            api_url: default_api_url(),
            rtmp_url: default_rtmp_url(),
            webrtc_url: default_webrtc_url(),
            hls_url: default_hls_url(),
            binary: default_mediamtx_binary(),
            config_path: default_mediamtx_config_path(),
            tls_cert: String::new(),
            tls_key: String::new(),
            rtmps_bind: String::new(),
            strict: false,
            rtmp_bind: default_loopback(),
            hls_bind: default_loopback(),
            webrtc_bind: default_loopback(),
            public_host: String::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            mediamtx: MediaMtxConfig::default(),
            destinations: Vec::new(),
            publish_keys: Vec::new(),
            shares: Vec::new(),
        }
    }
}

/// TOML booleans are strictly `true`/`false`, so a bare
/// `public-url-enabled = yes` fails at the parser level. Rewrite bare
/// yes/no/on/off values to quoted strings before parsing (the field's
/// `deserialize_bool_ish` accepts them); leaves everything else untouched.
fn expand_bare_bools(text: &str) -> String {
    const KEYS: [&str; 4] = [
        "public-url-enabled",
        "public_url_enabled",
        "enable-logging",
        "enable_logging",
    ];
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if !KEYS.iter().any(|k| trimmed.starts_with(k)) {
                return line.to_string();
            }
            let Some(eq) = line.find('=') else {
                return line.to_string();
            };
            let (before, after) = line.split_at(eq + 1);
            let rest = after;
            let lead = rest.len() - rest.trim_start().len();
            let value = rest.trim_start();
            // First token only, so trailing comments don't get in the way.
            let token_len = value
                .find(|c: char| c.is_whitespace())
                .unwrap_or(value.len());
            let token = &value[..token_len];
            let bare = matches!(
                token.to_ascii_lowercase().as_str(),
                "yes" | "no" | "on" | "off"
            );
            if bare {
                format!(
                    "{before}{ws}\"{token}\"{tail}",
                    ws = &rest[..lead],
                    tail = &value[token_len..]
                )
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let text = expand_bare_bools(&text);
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let cfg = Config::default();
        cfg.save(path)?;
        tracing::info!("created default config at {}", path.display());
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

/// MediaMTX config used in managed mode.
///
/// Security model:
/// - publish allowed from localhost + RFC1918 (your machines/LAN) or with a
///   CastBuddy stream key (hashed at rest) from anywhere;
/// - the public internet gets read-only playback;
/// - `strict` removes the wildcard path so only `ingest`, `player` and key
///   paths exist - unknown path names are refused;
/// - TLS certs (when configured) enable RTMPS on :1936 in "optional" mode.
///   When the RTMPS bind (`rtmps_bind`) is non-loopback, cleartext :1935 is
///   pinned to 127.0.0.1 regardless of `rtmp_bind`: remote publishers are
///   forced onto RTMPS, unencrypted ingests never leave the machine.
pub fn generated_mediamtx_config(cfg: &Config) -> String {
    let mt = &cfg.mediamtx;
    let mut y = String::new();
    y.push_str("# Generated by CastBuddy - do not edit while the daemon is running.\n");
    y.push_str(&format!(
        "logLevel: info\napi: yes\napiAddress: {}\n\n",
        yaml_str(&api_address(&mt.api_url))
    ));
    // Everything CastBuddy doesn't use is explicitly off (MediaMTX enables
    // RTSP and SRT by default, which would expose unauthenticated playback).
    y.push_str("rtsp: no\nsrt: no\nplayback: no\nmetrics: no\npprof: no\n\n");
    // Cleartext :1935 is pinned to loopback whenever RTMPS is exposed:
    // remote publishers must use RTMPS, so unencrypted ingests never leave
    // this machine (rtmp_bind cannot override that).
    let plain_bind = if mt.rtmps_public() {
        "127.0.0.1"
    } else {
        mt.rtmp_bind.as_str()
    };
    y.push_str(&format!(
        "rtmp: yes\nrtmpAddress: {}:1935\nhls: yes\nhlsAddress: {}:8888\n\
         webrtc: yes\nwebrtcAddress: {}:8889\nwebrtcLocalUDPAddress: {}:8189\n",
        plain_bind, mt.hls_bind, mt.webrtc_bind, mt.webrtc_bind
    ));
    // NOTE: do not lower hlsSegmentCount below 7 - MediaMTX refuses to serve
    // low-latency HLS with fewer ("Low-Latency HLS requires at least 7
    // segments"). Latency comes from segment/part durations (1s/200ms), not
    // from the segment count.
    if mt.tls_enabled() {
        y.push_str(&format!(
            "rtmpEncryption: optional\nrtmpsAddress: {}:1936\nrtmpServerCert: {}\nrtmpServerKey: {}\n",
            mt.rtmps_bind(),
            yaml_str(&mt.tls_cert),
            yaml_str(&mt.tls_key)
        ));
    }
    y.push_str("\nauthInternalUsers:\n");
    for entry in auth_internal_users(cfg) {
        y.push_str(&auth_entry_yml(&entry));
    }
    y.push_str("\npaths:\n");
    if mt.strict {
        y.push_str("  ingest:\n    source: publisher\n  player:\n    source: publisher\n");
        for k in cfg.publish_keys.iter().filter(|k| k.enabled) {
            y.push_str(&format!("  {}:\n    source: publisher\n", yaml_str(&k.path)));
        }
    } else {
        y.push_str("  all_others:\n    source: publisher\n");
    }
    y
}

fn yaml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `host:port` the generated config binds MediaMTX's control API on,
/// derived from `api_url` - the address CastBuddy itself calls - so an
/// override can never make the client and the generated config disagree.
/// Falls back to the default 127.0.0.1:9997 on anything unparseable.
fn api_address(api_url: &str) -> String {
    let rest = api_url
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let hostport = rest.split('/').next().unwrap_or("");
    if hostport.is_empty() {
        return "127.0.0.1:9997".into();
    }
    if hostport.contains(':') {
        // "host:port", or a bracketed IPv6 literal "[::1]:9997".
        hostport.to_string()
    } else {
        format!("{hostport}:9997")
    }
}

/// The internal-user database, shared by the generated config file and the
/// live-reload API patch so the two can never drift.
pub fn auth_internal_users(cfg: &Config) -> Vec<serde_json::Value> {
    let local = serde_json::json!({
        "user": "any",
        "pass": null,
        "ips": ["127.0.0.1", "::1", "192.168.0.0/16", "10.0.0.0/8", "172.16.0.0/12"],
        // Localhost/LAN may publish only to the well-known `ingest` and
        // `player` paths without credentials. Stream-key paths require the
        // key from ANYWHERE, including this machine. (MediaMTX permission
        // paths are exact-match strings; multiple paths = multiple entries.)
        "permissions": [
            {"action": "publish", "path": "ingest"},
            {"action": "publish", "path": "player"},
            {"action": "read"},
            {"action": "playback"},
            {"action": "api"},
        ],
    });
    let readonly = serde_json::json!({
        "user": "any",
        "pass": null,
        "ips": [],
        "permissions": [{"action": "read"}, {"action": "playback"}],
    });
    let mut users = vec![local, readonly];
    for k in cfg.publish_keys.iter().filter(|k| k.enabled) {
        users.push(serde_json::json!({
            "user": k.id,
            "pass": crate::keys::mediamtx_hash(&k.key),
            "ips": [],
            // v1.11: permission path is a plain (comma-separated) string,
            // NOT an array - an array makes MediaMTX reject the whole config.
            "permissions": [{"action": "publish", "path": k.path.clone()}],
        }));
    }
    users
}

/// Render one authInternalUsers entry as YAML (MediaMTX schema).
fn auth_entry_yml(e: &serde_json::Value) -> String {
    let mut s = format!(
        "  - user: {}\n",
        yaml_str(e["user"].as_str().unwrap_or("any"))
    );
    match e["pass"].as_str() {
        Some(p) => s.push_str(&format!("    pass: {}\n", yaml_str(p))),
        None => s.push_str("    pass:\n"),
    }
    let ips: Vec<String> = e["ips"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(yaml_str).collect())
        .unwrap_or_default();
    s.push_str(&format!("    ips: [{}]\n", ips.join(", ")));
    s.push_str("    permissions:\n");
    if let Some(perms) = e["permissions"].as_array() {
        for p in perms {
            s.push_str(&format!(
                "      - action: {}\n",
                p["action"].as_str().unwrap_or("read")
            ));
            if let Some(path) = p["path"].as_str() {
                if !path.is_empty() {
                    s.push_str(&format!("        path: {}\n", yaml_str(path)));
                }
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_logging_defaults_on_and_honors_false() {
        // Opt-out, not opt-in: an absent key must keep logging enabled.
        assert!(ServerConfig::default().enable_logging);

        let cfg: Config = toml::from_str("[server]\nenable-logging = false").unwrap();
        assert!(!cfg.server.enable_logging);

        // The snake_case alias and yes/no strings work like public-url-enabled.
        let cfg: Config = toml::from_str("[server]\nenable_logging = \"no\"").unwrap();
        assert!(!cfg.server.enable_logging);
        let cfg: Config = toml::from_str("[server]\nenable-logging = \"yes\"").unwrap();
        assert!(cfg.server.enable_logging);

        // Bare TOML-invalid yes/no are rewritten before parsing, like
        // public-url-enabled: the README documents "or yes/no".
        let cfg: Config =
            toml::from_str(&expand_bare_bools("[server]\nenable-logging = no\n")).unwrap();
        assert!(!cfg.server.enable_logging);
        let cfg: Config =
            toml::from_str(&expand_bare_bools("[server]\nenable_logging = yes # comment\n"))
                .unwrap();
        assert!(cfg.server.enable_logging);
        // Unrelated keys are left strictly alone.
        let cfg: Config = toml::from_str(&expand_bare_bools("[server]\nport = 8081\n")).unwrap();
        assert_eq!(cfg.server.port, 8081);
    }

    #[test]
    fn fresh_defaults_stream_out_of_the_box() {
        // The README quick start is `cargo run` + point OBS at localhost:1935;
        // that only works if a first-run config defaults to managed mode
        // (the daemon spawns MediaMTX itself).
        assert_eq!(MediaMtxConfig::default().mode, MediaMtxMode::Managed);
        let cfg: Config = toml::from_str("[server]\nport = 8081").unwrap();
        assert_eq!(cfg.mediamtx.mode, MediaMtxMode::Managed);
        // Explicit external stays external (Docker / pre-running MediaMTX).
        let cfg: Config = toml::from_str("[mediamtx]\nmode = \"external\"").unwrap();
        assert_eq!(cfg.mediamtx.mode, MediaMtxMode::External);
    }

    #[test]
    fn api_address_follows_api_url() {
        // The generated MediaMTX config binds the API where CastBuddy will
        // actually call it - overriding api_url must stay consistent.
        assert_eq!(api_address("http://127.0.0.1:9997"), "127.0.0.1:9997");
        assert_eq!(api_address("https://127.0.0.1:9443/"), "127.0.0.1:9443");
        assert_eq!(api_address("http://10.0.0.4"), "10.0.0.4:9997");
        assert_eq!(api_address("http://[::1]:9998"), "[::1]:9998");
        assert_eq!(api_address("nonsense"), "nonsense:9997");
        assert_eq!(api_address(""), "127.0.0.1:9997");

        let cfg: Config = toml::from_str("[mediamtx]\napi_url = \"http://127.0.0.1:9443\"").unwrap();
        let yml = generated_mediamtx_config(&cfg);
        assert!(yml.contains("apiAddress: \"127.0.0.1:9443\""), "{yml}");
    }
}
