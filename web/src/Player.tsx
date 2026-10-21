import { useCallback, useEffect, useRef, useState } from "react";
import { api, BrowseReply, PlayerStatus } from "./api";

function fmt(t: number): string {
  if (!Number.isFinite(t) || t < 0) t = 0;
  const m = Math.floor(t / 60);
  const s = Math.floor(t % 60);
  return `${m}:${s.toString().padStart(2, "0")}`;
}

export default function Player() {
  const [st, setSt] = useState<PlayerStatus | null>(null);
  const [browse, setBrowse] = useState<BrowseReply | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [scrub, setScrub] = useState<number | null>(null);
  const seekTimer = useRef<number | null>(null);

  const refresh = useCallback(async () => {
    try {
      setSt(await api.playerStatus());
      setErr(null);
    } catch (e) {
      setErr(String(e));
    }
  }, []);

  const loadDir = useCallback(async (dir: string) => {
    try {
      setBrowse(await api.playerBrowse(dir));
      setErr(null);
    } catch (e) {
      setErr(String(e));
    }
  }, []);

  useEffect(() => {
    refresh();
    loadDir("");
    const t = setInterval(refresh, 2000);
    return () => clearInterval(t);
  }, [refresh, loadDir]);

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      await refresh();
      setErr(null);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doSeek = (pos: number) => {
    setScrub(pos);
    if (seekTimer.current !== null) window.clearTimeout(seekTimer.current);
    seekTimer.current = window.setTimeout(() => {
      seekTimer.current = null;
      const p = scrub === null ? pos : pos;
      run(() => api.playerSeek(p));
      setScrub(null);
    }, 400);
  };

  const active = st?.active ?? false;
  const pos = scrub ?? st?.time_pos ?? 0;
  const dur = st?.duration ?? null;

  return (
    <div className="card">
      <div className="card-title">
        <span>Player</span>
        <span className={active ? "dot dot-live" : "dot"} />
      </div>

      {err && <div className="error">{err}</div>}

      {active ? (
        <>
          <div className="player-meta">
            <span className="player-title" title={st?.title ?? ""}>
              {st?.title ?? st?.source ?? "playing"}
            </span>
            <span className="muted">{st?.paused ? "PAUSED" : "PLAYING"}</span>
          </div>
          <div className="player-seek">
            <span className="mono player-time">{fmt(pos)}</span>
            <input
              type="range"
              min={0}
              max={dur && dur > 0 ? dur : Math.max(pos + 1, 1)}
              step={0.5}
              value={pos}
              disabled={dur === null}
              onChange={(e) => doSeek(parseFloat(e.target.value))}
            />
            <span className="mono player-time">
              {dur !== null ? fmt(dur) : "--:--"}
            </span>
          </div>
          <div className="player-transport">
            <button
              className="btn btn-small btn-primary"
              onClick={() => run(() => (st?.paused ? api.playerResume() : api.playerPause()))}
              disabled={busy}
            >
              {st?.paused ? "Resume" : "Pause"}
            </button>
            <button
              className="btn btn-small btn-danger"
              onClick={() => run(() => api.playerStop())}
              disabled={busy}
            >
              Stop
            </button>
            <button
              className="btn btn-small"
              onClick={() => run(() => api.playerMonitor(!st?.monitor))}
              disabled={busy}
            >
              Monitoring: {st?.monitor ? "ON" : "OFF"}
            </button>
          </div>
        </>
      ) : (
        <div className="muted">Idle — pick a file below to stream.</div>
      )}

      <div className="player-browser">
        <div className="player-path mono">{browse?.dir || "/"}</div>
        <div className="player-entries">
          {browse?.parent !== null && browse?.parent !== undefined && (
            <button
              className="btn btn-small"
              onClick={() => loadDir(browse.parent ?? "")}
            >
              ..
            </button>
          )}
          {(browse?.entries ?? []).map((e) => {
            const next = browse?.dir ? `${browse.dir}/${e.name}` : e.name;
            return e.is_dir ? (
              <button
                key={e.name}
                className="btn btn-small"
                onClick={() => loadDir(next)}
              >
                {e.name}/
              </button>
            ) : (
              <button
                key={e.name}
                className="btn btn-small btn-primary"
                disabled={busy}
                onClick={() => run(() => api.playerPlay(next, st?.monitor ?? false))}
              >
                {e.name}
              </button>
            );
          })}
          {(browse?.entries.length ?? 0) === 0 && (
            <span className="muted">empty folder</span>
          )}
        </div>
      </div>
    </div>
  );
}
