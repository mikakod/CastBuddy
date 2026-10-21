// Typed client for the CastBuddy REST API.

export interface DestinationStatus {
  id: string;
  name: string;
  url: string;
  key: string;
  enabled: boolean;
  state: "running" | "backoff" | "stopped";
  last_error: string | null;
  logs: string[];
}

export interface StatusReply {
  mediamtx_reachable: boolean;
  mediamtx_error: string | null;
  mediamtx_mode: string;
  ingest_live: boolean;
  ingest_path: string | null;
  ingest_viewers: number | null;
  rtmp_url: string;
  /** Port of the public share listener. */
  public_port: number;
  /** Configured public base URL for share links (e.g. "http://example.com"),
   *  or null to build links from the dashboard's own hostname + public_port. */
  public_url: string | null;
  /** Port of MediaMTX's HLS listener (browser preview fallback). */
  hls_port: number;
  destinations: DestinationStatus[];
}

export interface Destination {
  id: string;
  name: string;
  url: string;
  key: string;
  enabled: boolean;
}

export interface DestinationInput {
  name: string;
  url: string;
  key: string;
}

export interface SourcesReply {
  obs_url: string;
  obs_key_hint: string;
  rtmps: boolean;
  strict: boolean;
  ingest_live: boolean;
  ingest_path: string | null;
}

export interface VlcReply {
  command: string;
  note: string;
}

export interface PlayerStatus {
  active: boolean;
  source: string | null;
  title: string | null;
  paused: boolean;
  time_pos: number;
  duration: number | null;
  monitor: boolean;
  error: string | null;
}

export interface BrowseEntry {
  name: string;
  is_dir: boolean;
}

export interface BrowseReply {
  /** Normalized relative dir ("" = media_dir root). */
  dir: string;
  parent: string | null;
  entries: BrowseEntry[];
}

export interface PublishKey {
  id: string;
  name: string;
  path: string;
  key: string;
  /** Unix seconds. */
  created: number;
  enabled: boolean;
  /** Ready-made publisher URL with credentials embedded (ffmpeg/VLC). */
  obs_url: string;
  /** OBS Server field (credentials embedded, no path). */
  obs_server: string;
  /** OBS Stream Key field (becomes the MediaMTX path). */
  obs_key: string;
}

export interface Share {
  id: string;
  name: string;
  /** "live" | "file" | "folder" */
  kind: string;
  target: string;
  created: number;
  /** Unix seconds; 0 = never. */
  expires: number;
  /** URL path on the public listener. */
  path: string;
}

export interface ShareInput {
  name: string;
  kind: string;
  target: string;
  expires: number;
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(`/api/v1${path}`, {
    headers: { "content-type": "application/json" },
    ...init,
  });
  if (!resp.ok) {
    const text = await resp.text().catch(() => resp.statusText);
    throw new Error(`${resp.status}: ${text}`);
  }
  if (resp.status === 204) return undefined as T;
  return resp.json() as Promise<T>;
}

export const api = {
  status: () => req<StatusReply>("/status"),
  sources: () => req<SourcesReply>("/sources"),
  vlcCommand: (file?: string) =>
    req<VlcReply>(
      file ? `/sources/vlc?file=${encodeURIComponent(file)}` : "/sources/vlc",
    ),
  destinations: () => req<Destination[]>("/destinations"),
  createDestination: (input: DestinationInput) =>
    req<Destination>("/destinations", {
      method: "POST",
      body: JSON.stringify(input),
    }),
  updateDestination: (id: string, input: DestinationInput) =>
    req<Destination>(`/destinations/${encodeURIComponent(id)}`, {
      method: "PUT",
      body: JSON.stringify(input),
    }),
  enableDestination: (id: string) =>
    req<Destination>(`/destinations/${encodeURIComponent(id)}/enable`, {
      method: "POST",
    }),
  disableDestination: (id: string) =>
    req<Destination>(`/destinations/${encodeURIComponent(id)}/disable`, {
      method: "POST",
    }),
  deleteDestination: (id: string) =>
    req<void>(`/destinations/${encodeURIComponent(id)}`, { method: "DELETE" }),
  // ---- publish (stream) keys ----
  publishKeys: () => req<PublishKey[]>("/publish-keys"),
  createPublishKey: (name: string) =>
    req<PublishKey>("/publish-keys", {
      method: "POST",
      body: JSON.stringify({ name }),
    }),
  deletePublishKey: (id: string) =>
    req<void>(`/publish-keys/${encodeURIComponent(id)}`, { method: "DELETE" }),
  enablePublishKey: (id: string) =>
    req<PublishKey>(`/publish-keys/${encodeURIComponent(id)}/enable`, {
      method: "POST",
    }),
  disablePublishKey: (id: string) =>
    req<PublishKey>(`/publish-keys/${encodeURIComponent(id)}/disable`, {
      method: "POST",
    }),
  // ---- share links ----
  shares: () => req<Share[]>("/shares"),
  createShare: (input: ShareInput) =>
    req<Share>("/shares", { method: "POST", body: JSON.stringify(input) }),
  deleteShare: (id: string) =>
    req<void>(`/shares/${encodeURIComponent(id)}`, { method: "DELETE" }),
  // ---- built-in player ----
  playerStatus: () => req<PlayerStatus>("/player/status"),
  playerBrowse: (dir: string) =>
    req<BrowseReply>(`/player/browse?dir=${encodeURIComponent(dir)}`),
  playerPlay: (file: string, monitor: boolean) =>
    req<PlayerStatus>("/player/play", {
      method: "POST",
      body: JSON.stringify({ file, monitor }),
    }),
  playerPause: () => req<PlayerStatus>("/player/pause", { method: "POST" }),
  playerResume: () => req<PlayerStatus>("/player/resume", { method: "POST" }),
  playerSeek: (position: number) =>
    req<PlayerStatus>("/player/seek", {
      method: "POST",
      body: JSON.stringify({ position }),
    }),
  playerStop: () => req<PlayerStatus>("/player/stop", { method: "POST" }),
  playerMonitor: (monitor: boolean) =>
    req<PlayerStatus>("/player/monitor", {
      method: "POST",
      body: JSON.stringify({ monitor }),
    }),
};

/** Minimal WHEP client: POST our SDP offer to the daemon's preview proxy. */
export async function whepPlay(
  video: HTMLVideoElement,
  path: string,
): Promise<RTCPeerConnection> {
  const pc = new RTCPeerConnection({ iceServers: [] });
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });
  pc.ontrack = (ev) => {
    if (video.srcObject !== ev.streams[0]) {
      video.srcObject = ev.streams[0];
    }
  };
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  // Wait for ICE gathering to finish so the offer contains all candidates
  // (MediaMTX WHEP answers non-trickle offers).
  await new Promise<void>((resolve) => {
    if (pc.iceGatheringState === "complete") return resolve();
    const t = setTimeout(resolve, 2000);
    pc.onicegatheringstatechange = () => {
      if (pc.iceGatheringState === "complete") {
        clearTimeout(t);
        resolve();
      }
    };
  });
  const resp = await fetch(`/api/v1/preview/${path}`, {
    method: "POST",
    headers: { "content-type": "application/sdp" },
    body: pc.localDescription?.sdp ?? "",
  });
  if (!resp.ok) throw new Error(`WHEP failed: ${resp.status}`);
  const answer = await resp.text();
  await pc.setRemoteDescription({ type: "answer", sdp: answer });
  return pc;
}
