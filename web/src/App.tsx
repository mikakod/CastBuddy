import { useCallback, useEffect, useRef, useState } from "react";
import Hls from "hls.js";
import {
  api,
  Destination,
  DestinationStatus,
  PublishKey,
  Share,
  SourcesReply,
  StatusReply,
  whepPlay,
} from "./api";
import Player from "./Player";

const POLL_MS = 2000;

function usePoll<T>(fn: () => Promise<T>): [T | null, string | null, () => void] {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const fnRef = useRef(fn);
  fnRef.current = fn;

  const refresh = useCallback(() => {
    fnRef
      .current()
      .then((d) => {
        setData(d);
        setError(null);
      })
      .catch((e: Error) => setError(e.message));
  }, []);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, POLL_MS);
    return () => clearInterval(t);
  }, [refresh]);

  return [data, error, refresh];
}

function Pill({ ok, label }: { ok: boolean; label: string }) {
  return (
    <span className={`pill ${ok ? "pill-ok" : "pill-bad"}`}>
      <span className="dot" />
      {label}
    </span>
  );
}

function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      className="btn btn-small"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
        } catch {
          // Clipboard API needs a secure context; fall back to a prompt.
          window.prompt("Copy manually:", text);
        }
        setDone(true);
        setTimeout(() => setDone(false), 1200);
      }}
    >
      {done ? "Copied!" : label}
    </button>
  );
}

function Preview({ path, hlsPort }: { path: string; hlsPort: number }) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const pcRef = useRef<RTCPeerConnection | null>(null);
  const hlsRef = useRef<Hls | null>(null);
  // Default to HLS: WebRTC physically cannot carry the AAC audio our RTMP
  // pipeline produces (MediaMTX only passes Opus/G722/G711 through), so a
  // WebRTC preview is always video-only and silent. HLS keeps the audio.
  const [mode, setMode] = useState<"webrtc" | "hls">("hls");
  const [playing, setPlaying] = useState(false);
  const [info, setInfo] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const stop = useCallback(() => {
    pcRef.current?.close();
    pcRef.current = null;
    hlsRef.current?.destroy();
    hlsRef.current = null;
    const v = videoRef.current;
    if (v) {
      v.srcObject = null;
      v.removeAttribute("src");
      v.load();
    }
    setPlaying(false);
  }, []);

  const startHls = useCallback(() => {
    const video = videoRef.current;
    if (!video) return;
    const src = `http://${window.location.hostname}:${hlsPort}/${path}/index.m3u8`;
    if (video.canPlayType("application/vnd.apple.mpegurl")) {
      video.src = src; // Safari / native HLS
    } else {
      const hls = new Hls({ maxBufferLength: 10 });
      hlsRef.current = hls;
      hls.loadSource(src);
      hls.attachMedia(video);
      hls.on(Hls.Events.ERROR, (_ev, data) => {
        if (data.fatal) setError(`HLS: ${data.details}`);
      });
    }
    setPlaying(true);
    setInfo(
      mode === "webrtc" ? "WebRTC can't handle this stream — using HLS instead." : null,
    );
  }, [hlsPort, path, mode]);

  const start = useCallback(async () => {
    if (!videoRef.current) return;
    setInfo(null);
    setError(null);
    if (mode === "hls") {
      startHls();
      return;
    }
    try {
      const pc = await whepPlay(videoRef.current, path);
      pcRef.current = pc;
      setPlaying(true);
      // MediaMTX tears WebRTC sessions down when the stream isn't WebRTC-safe
      // (most often: H264 with B-frames). Watch the connection state and fall
      // back to HLS instead of showing a black frame forever.
      let discTimer: number | undefined;
      const failOver = () => {
        if (pcRef.current !== pc) return;
        pcRef.current = null;
        pc.close();
        if (videoRef.current) videoRef.current.srcObject = null;
        setMode("hls");
        startHls();
      };
      pc.onconnectionstatechange = () => {
        switch (pc.connectionState) {
          case "disconnected":
            discTimer = window.setTimeout(failOver, 3000);
            break;
          case "connected":
            clearTimeout(discTimer);
            break;
          case "failed":
          case "closed":
            failOver();
            break;
        }
      };
    } catch (e) {
      setError((e as Error).message);
      setPlaying(false);
    }
  }, [mode, path, startHls]);

  useEffect(() => stop, [stop]);

  return (
    <div className="preview">
      <video ref={videoRef} autoPlay playsInline muted />
      <div className="preview-controls">
        <div className="seg">
          <button
            className={`seg-btn${mode === "webrtc" ? " active" : ""}`}
            onClick={() => {
              if (playing) stop();
              setMode("webrtc");
            }}
          >
            WebRTC
          </button>
          <button
            className={`seg-btn${mode === "hls" ? " active" : ""}`}
            onClick={() => {
              if (playing) stop();
              setMode("hls");
            }}
          >
            HLS
          </button>
        </div>
        {playing ? (
          <button className="btn" onClick={stop}>
            Stop preview
          </button>
        ) : (
          <button className="btn btn-primary" onClick={start}>
            Start preview
          </button>
        )}
        {info && <span className="hint">{info}</span>}
        {error && <span className="error-text">{error}</span>}
      </div>
    </div>
  );
}

function Sources() {
  const [sources, setSources] = useState<SourcesReply | null>(null);
  const [vlc, setVlc] = useState<string>("");
  const [vlcNote, setVlcNote] = useState("");

  useEffect(() => {
    api.sources().then(setSources).catch(() => {});
    api
      .vlcCommand()
      .then((r) => {
        setVlc(r.command);
        setVlcNote(r.note);
      })
      .catch(() => {});
  }, []);

  if (!sources) return <section className="card">Loading sources…</section>;
  const obsServer = sources.obs_url;

  return (
    <section className="card">
      <h2>Sources</h2>
      <div className="source-block">
        <h3>OBS / any RTMP encoder</h3>
        <p className="hint">OBS: Settings → Stream → Custom, paste:</p>
        <div className="copy-row">
          <code>{obsServer}</code>
          <CopyButton text={obsServer} />
        </div>
        <p className="hint">
          Stream key: <code>{sources.obs_key_hint}</code> (anything)
          {sources.ingest_live && (
            <span className="live-badge"> · ingest LIVE ({sources.ingest_path})</span>
          )}
        </p>
        {sources.rtmps && (
          <p className="hint">
            RTMPS is on: this URL is for OBS on <em>this</em> PC (cleartext,
            loopback-only). For a guest PC over the internet, use a URL from
            the Stream keys card (<code>rtmps://…</code>).
          </p>
        )}
      </div>
      <div className="source-block">
        <h3>VLC (silent locally, full volume on stream)</h3>
        <pre className="command">{vlc}</pre>
        <div className="copy-row">
          <CopyButton text={vlc} label="Copy VLC command" />
          {vlcNote && <span className="hint">{vlcNote}</span>}
        </div>
      </div>
    </section>
  );
}

function Destinations({
  statuses,
  onChanged,
}: {
  statuses: DestinationStatus[];
  onChanged: () => void;
}) {
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Destination | null>(null);

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setFormError(null);
    try {
      await fn();
      onChanged();
    } catch (e) {
      setFormError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const submit = (ev: React.FormEvent) => {
    ev.preventDefault();
    const input = { name, url, key };
    if (editing) {
      run(() => api.updateDestination(editing.id, input));
    } else {
      run(() => api.createDestination(input));
    }
    setName("");
    setUrl("");
    setKey("");
    setEditing(null);
  };

  const presets: [string, string][] = [
    ["YouTube", "rtmp://a.rtmp.youtube.com/live2"],
    ["Twitch", "rtmp://live.twitch.tv/app"],
    ["Custom", ""],
  ];

  return (
    <section className="card">
      <h2>Destinations {statuses.length > 0 && <span className="count">{statuses.length}</span>}</h2>
      {statuses.length === 0 && <p className="hint">No destinations yet.</p>}
      <ul className="dest-list">
        {statuses.map((d) => (
          <li key={d.id} className={`dest ${d.state}`}>
            <div className="dest-main">
              <span className={`state-dot state-${d.state}`} />
              <div>
                <strong>{d.name}</strong>{" "}
                <span className="hint mono">{d.url}</span>
                {d.last_error && (
                  <div className="error-text">{d.last_error}</div>
                )}
              </div>
            </div>
            <div className="dest-actions">
              <span className={`state-label state-${d.state}`}>{d.state}</span>
              {d.enabled ? (
                <button
                  className="btn btn-small"
                  onClick={() => run(() => api.disableDestination(d.id))}
                  disabled={busy}
                >
                  Disable
                </button>
              ) : (
                <button
                  className="btn btn-small btn-primary"
                  onClick={() => run(() => api.enableDestination(d.id))}
                  disabled={busy}
                >
                  Enable
                </button>
              )}
              <button
                className="btn btn-small"
                onClick={() => {
                  setEditing(d);
                  setName(d.name);
                  setUrl(d.url);
                  setKey(d.key);
                }}
              >
                Edit
              </button>
              <button
                className="btn btn-small btn-danger"
                onClick={() => {
                  if (confirm(`Delete destination "${d.name}"?`)) {
                    run(() => api.deleteDestination(d.id));
                  }
                }}
                disabled={busy}
              >
                Delete
              </button>
            </div>
            {d.logs.length > 0 && (
              <details className="logs">
                <summary>Logs</summary>
                <pre>{d.logs.join("\n")}</pre>
              </details>
            )}
          </li>
        ))}
      </ul>

      <form className="dest-form" onSubmit={submit}>
        <h3>{editing ? `Edit: ${editing.name}` : "Add destination"}</h3>
        {presets.length > 0 && (
          <div className="presets">
            {presets.map(([label, presetUrl]) => (
              <button
                key={label}
                type="button"
                className="btn btn-small"
                onClick={() => setUrl(presetUrl)}
              >
                {label}
              </button>
            ))}
          </div>
        )}
        <input
          placeholder="Name (e.g. YouTube main)"
          value={name}
          onChange={(e) => setName(e.target.value)}
          required
        />
        <input
          placeholder="rtmp://…"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          required
        />
        <input
          placeholder="Stream key (optional)"
          value={key}
          onChange={(e) => setKey(e.target.value)}
        />
        {formError && <div className="error-text">{formError}</div>}
        <div className="copy-row">
          <button className="btn btn-primary" type="submit" disabled={busy}>
            {editing ? "Save" : "Add"}
          </button>
          {editing && (
            <button
              type="button"
              className="btn"
              onClick={() => {
                setEditing(null);
                setName("");
                setUrl("");
                setKey("");
              }}
            >
              Cancel
            </button>
          )}
        </div>
      </form>
    </section>
  );
}

function KeysCard() {
  const [keys, setKeys] = useState<PublishKey[]>([]);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(() => {
    api
      .publishKeys()
      .then((k) => {
        setKeys(k);
        setErr(null);
      })
      .catch((e: Error) => setErr(e.message));
  }, []);

  useEffect(load, [load]);

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      load();
      setErr(null);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const submit = (ev: React.FormEvent) => {
    ev.preventDefault();
    const trimmed = name.trim();
    if (!trimmed) return;
    run(() => api.createPublishKey(trimmed));
    setName("");
  };

  return (
    <section className="card">
      <h2>Stream keys</h2>
      <p className="hint">
        Give a guest OBS its own publish URL, revoke anytime. Only key hashes
        are stored.
      </p>
      {err && <div className="error-text">{err}</div>}
      <form className="dest-form" onSubmit={submit}>
        <input
          placeholder="name, e.g. guest-obs"
          value={name}
          onChange={(e) => setName(e.target.value)}
          maxLength={64}
        />
        <button
          className="btn btn-primary"
          type="submit"
          disabled={busy || !name.trim()}
        >
          Generate
        </button>
      </form>
      <ul className="dest-list">
        {keys.map((k) => (
          <li key={k.id} className={`dest ${k.enabled ? "running" : "stopped"}`}>
            <div className="dest-main">
              <span
                className={`state-dot state-${k.enabled ? "running" : "stopped"}`}
              />
              <div>
                <strong>{k.name}</strong>{" "}
                <span className="hint mono">/{k.path}</span>
                <div className="copy-row key-url">
                  <span className="hint">OBS Server:</span>
                  <code className="mono key-secret">{k.obs_server}</code>
                  <CopyButton text={k.obs_server} />
                </div>
                <div className="copy-row key-url">
                  <span className="hint">OBS Stream Key:</span>
                  <code className="mono key-secret">{k.obs_key}</code>
                  <CopyButton text={k.obs_key} />
                </div>
                <p className="hint key-obs-hint">
                  In OBS leave{" "}
                  <strong>&ldquo;Use authentication&rdquo; unchecked</strong> — the
                  key carries the credentials.
                </p>
                <div className="copy-row key-url">
                  <span className="hint">Full URL (ffmpeg/VLC):</span>
                  <code className="mono key-secret">{k.obs_url}</code>
                  <CopyButton text={k.obs_url} label="Copy URL" />
                </div>
              </div>
            </div>
            <div className="dest-actions">
              <span
                className={`state-label state-${k.enabled ? "running" : "stopped"}`}
              >
                {k.enabled ? "active" : "disabled"}
              </span>
              {k.enabled ? (
                <button
                  className="btn btn-small"
                  disabled={busy}
                  onClick={() => run(() => api.disablePublishKey(k.id))}
                >
                  Disable
                </button>
              ) : (
                <button
                  className="btn btn-small btn-primary"
                  disabled={busy}
                  onClick={() => run(() => api.enablePublishKey(k.id))}
                >
                  Enable
                </button>
              )}
              <button
                className="btn btn-small btn-danger"
                disabled={busy}
                onClick={() => {
                  if (confirm(`Revoke key "${k.name}"?`)) {
                    run(() => api.deletePublishKey(k.id));
                  }
                }}
              >
                Revoke
              </button>
            </div>
          </li>
        ))}
      </ul>
      {keys.length === 0 && (
        <p className="hint">No keys. Local publishing still works without one.</p>
      )}
    </section>
  );
}

function SharesCard({
  publicPort,
  publicUrl,
}: {
  publicPort: number;
  publicUrl: string | null;
}) {
  const [shares, setShares] = useState<Share[]>([]);
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"live" | "file" | "folder">("live");
  const [target, setTarget] = useState("");
  const [days, setDays] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(() => {
    api
      .shares()
      .then((s) => {
        setShares(s);
        setErr(null);
      })
      .catch((e: Error) => setErr(e.message));
  }, []);

  useEffect(load, [load]);

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      load();
      setErr(null);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const submit = (ev: React.FormEvent) => {
    ev.preventDefault();
    const t = target.trim();
    if (!name.trim() || !t) return;
    const d = parseFloat(days);
    const expires =
      Number.isFinite(d) && d > 0
        ? Math.floor(Date.now() / 1000) + Math.round(d * 86400)
        : 0;
    run(() => api.createShare({ name: name.trim(), kind, target: t, expires }));
    setName("");
    setTarget("");
    setDays("");
  };

  const base = publicUrl ?? `http://${window.location.hostname}:${publicPort}`;
  const link = (s: Share) => `${base}${s.path}`;

  return (
    <section className="card">
      <h2>Share links</h2>
      <p className="hint">
        {publicUrl
          ? `Unguessable links at ${base}.`
          : `Unguessable links on port ${publicPort}.`}{" "}
        Live pages play in the browser;
        files stream or download. Anyone with the link can watch until you
        revoke it.
      </p>
      {err && <div className="error-text">{err}</div>}
      <form className="dest-form" onSubmit={submit}>
        <input
          placeholder="name, e.g. movie night"
          value={name}
          onChange={(e) => setName(e.target.value)}
          maxLength={64}
        />
        <select value={kind} onChange={(e) => setKind(e.target.value as typeof kind)}>
          <option value="live">live watch page</option>
          <option value="file">single file</option>
          <option value="folder">folder</option>
        </select>
        <input
          placeholder={
            kind === "live"
              ? "media path, e.g. ingest"
              : "path under media_dir, e.g. clip.mp4"
          }
          value={target}
          onChange={(e) => setTarget(e.target.value)}
        />
        <input
          className="days"
          placeholder="expires (days)"
          value={days}
          onChange={(e) => setDays(e.target.value)}
          inputMode="decimal"
        />
        <button
          className="btn btn-primary"
          type="submit"
          disabled={busy || !name.trim() || !target.trim()}
        >
          Create
        </button>
      </form>
      <ul className="dest-list">
        {shares.map((s) => (
          <li key={s.id} className="dest running">
            <div className="dest-main">
              <span className="state-dot state-running" />
              <div>
                <strong>{s.name}</strong>{" "}
                <span className="hint">{s.kind}</span>{" "}
                <span className="hint mono">→ {s.target}</span>
                <div className="copy-row key-url">
                  <code className="mono key-secret">{link(s)}</code>
                  <CopyButton text={link(s)} label="Copy link" />
                </div>
                {s.kind === "live" && (
                  <div className="copy-row key-url">
                    <code className="mono key-secret">{link(s)}/index.m3u8</code>
                    <CopyButton
                      text={`${link(s)}/index.m3u8`}
                      label="Copy player URL"
                    />
                  </div>
                )}
              </div>
            </div>
            <div className="dest-actions">
              <button
                className="btn btn-small btn-danger"
                disabled={busy}
                onClick={() => {
                  if (confirm(`Revoke share "${s.name}"?`)) {
                    run(() => api.deleteShare(s.id));
                  }
                }}
              >
                Revoke
              </button>
            </div>
          </li>
        ))}
      </ul>
      {shares.length === 0 && (
        <p className="hint">No shares yet. Create one above.</p>
      )}
    </section>
  );
}

export default function App() {
  const [status, statusErr, refresh] = usePoll<StatusReply>(() => api.status());

  const livePath = status?.ingest_live ? status.ingest_path : null;

  return (
    <div className="app">
      <header>
        <h1>
          Cast<span className="accent">Buddy</span>
        </h1>
        <div className="header-pills">
          {status ? (
            <>
              <Pill
                ok={status.mediamtx_reachable}
                label={
                  status.mediamtx_reachable
                    ? `MediaMTX (${status.mediamtx_mode})`
                    : "MediaMTX unreachable"
                }
              />
              <Pill
                ok={status.ingest_live}
                label={status.ingest_live ? `Ingest: ${status.ingest_path}` : "No ingest"}
              />
              {status.ingest_viewers != null && status.ingest_viewers > 0 && (
                <Pill ok label={`${status.ingest_viewers} viewer(s)`} />
              )}
            </>
          ) : (
            <Pill ok={false} label="connecting…" />
          )}
          {statusErr && <span className="error-text">{statusErr}</span>}
        </div>
      </header>

      <main>
        <div className="col">
          {livePath ? (
            <Preview path={livePath} hlsPort={status?.hls_port ?? 8888} />
          ) : (
            <section className="card preview-empty">
              <h2>Preview</h2>
              <p className="hint">
                Nothing live right now. Start OBS or VLC to preview.
              </p>
            </section>
          )}
          <Sources />
          <Player />
        </div>
        <div className="col">
          <KeysCard />
          <SharesCard
            publicPort={status?.public_port ?? 49989}
            publicUrl={status?.public_url ?? null}
          />
          <Destinations
            statuses={status?.destinations ?? []}
            onChanged={refresh}
          />
        </div>
      </main>

      <footer>
        <span className="hint">
          CastBuddy · config: <code>castbuddy.toml</code>
        </span>
      </footer>
    </div>
  );
}
