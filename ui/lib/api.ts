// REST client + WS auth helpers for the open-sandbox dev console.
// Both auth paths (Authorization header for REST, Sec-WebSocket-Protocol
// subprotocol for WS) are documented in CONTRACTS.md § WebSocket auth.

// Must match WS_AUTH_PROTOCOL_SENTINEL in crates/contracts/src/constants.rs.
export const WS_AUTH_SENTINEL = "open-sandbox.v1";

/** The status strings the controller serializes for `SandboxInfo.status`.
 *  Source of truth: `sandbox_state_to_str` in `crates/controller/src/grpc.rs`.
 *  The intersection with `string & {}` keeps autocompletion of the known
 *  variants but still accepts unknown strings — the contract's
 *  `SandboxState` enum is `#[non_exhaustive]`, so a future variant we
 *  haven't mirrored here shouldn't crash the UI. */
export type SandboxStatus =
  | "creating"
  | "running"
  | "stopping"
  | "stopped"
  | "failed"
  | "pausing"
  | "paused"
  | "unpausing"
  | "unknown"
  // eslint-disable-next-line @typescript-eslint/ban-types
  | (string & {});

/** True when the sandbox is ready to accept exec / serve traffic.
 *  Centralizing the predicate so we don't sprinkle string literals
 *  across components (and so we can refine the rule without touching
 *  every call site). */
export function isRunningStatus(s: SandboxStatus): boolean {
  return s === "running";
}

export interface Sandbox {
  sandbox_id: string;
  agent_id: string;
  subdomain: string;
  status: SandboxStatus;
  error?: string | null;
}

export interface ApiConfig {
  base: string; // e.g. "http://127.0.0.1:8081"
  key: string;
}

function trimBase(s: string) {
  return s.replace(/\/+$/, "");
}

export function wsBase(httpBase: string): string {
  // Case-insensitive scheme rewrite so a pasted `HTTPS://…` becomes
  // `wss://…` rather than blowing up `new WebSocket()`.
  return trimBase(httpBase).replace(/^https?/i, (m) =>
    m.toLowerCase() === "https" ? "wss" : "ws",
  );
}

/** Derive the public reverse-proxy URL for a sandbox given the api
 *  gateway base. The proxy serves `<subdomain>.<host>:<proxy_port>`;
 *  for local dev (loopback hosts) we substitute `localtest.me`, which
 *  resolves to 127.0.0.1 without any /etc/hosts edits (per
 *  PLAN_DEV_MODE.md probe #4). Proxy port defaults to 8080 when the
 *  api base uses the dev convention (8081 → 8080 swap); otherwise we
 *  reuse the api host's port verbatim (production single-port
 *  deployments). */
export function publicUrl(apiBase: string, subdomain: string): string {
  try {
    const u = new URL(trimBase(apiBase));
    const isLocal =
      u.hostname === "127.0.0.1" ||
      u.hostname === "localhost" ||
      u.hostname === "::1" ||
      u.hostname === "[::1]";
    const host = isLocal ? "localtest.me" : u.hostname;
    // 8081 (api) → 8080 (proxy) is the convention dev-up.sh uses; any
    // other port is left alone so production deployments terminating
    // the api and the proxy on the same hostname:port both work.
    const port = u.port === "8081" ? "8080" : u.port;
    const portStr = port ? `:${port}` : "";
    return `${u.protocol}//${subdomain}.${host}${portStr}`;
  } catch {
    return `http://${subdomain}.localtest.me:8080`;
  }
}

// base64url-no-padding — keeps the API key inside the RFC 7230 token
// grammar that browsers enforce on subprotocol values.
export function b64urlEncode(s: string): string {
  const bytes = new TextEncoder().encode(s);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

export class ApiError extends Error {
  constructor(
    message: string,
    // status === 0 is the convention for transport-level failures
    // (network unreachable, CORS rejection, mixed-content block, DNS,
    // TLS, etc.) — anything where fetch() itself threw and we never
    // got an HTTP response. UI code can branch on this to render
    // actionable messages instead of the raw "TypeError: Failed to
    // fetch".
    public readonly status: number,
    public readonly errorCode?: string,
  ) {
    super(message);
  }
}

/** Run a fetch and re-shape transport-level errors as ApiError(…, 0).
 *  Without this wrapper a network drop / CORS block surfaces as a raw
 *  TypeError that callers print verbatim ("TypeError: Failed to fetch"). */
async function safeFetch(input: string, init?: RequestInit): Promise<Response> {
  try {
    return await fetch(input, init);
  } catch (e) {
    const detail = e instanceof Error ? e.message : String(e);
    throw new ApiError(
      `network error: ${detail} (check API base URL, CORS, and that the api gateway is running)`,
      0,
    );
  }
}

async function request<T>(
  cfg: ApiConfig,
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const headers: Record<string, string> = {
    Authorization: `Bearer ${cfg.key}`,
    ...(init.body && !init.headers
      ? { "Content-Type": "application/json" }
      : {}),
    ...((init.headers as Record<string, string>) ?? {}),
  };
  const res = await safeFetch(trimBase(cfg.base) + path, { ...init, headers });
  const text = await res.text();
  if (!res.ok) {
    let code: string | undefined;
    try {
      const j = JSON.parse(text);
      code = j.error_code;
      throw new ApiError(j.error ?? text, res.status, code);
    } catch (e) {
      if (e instanceof ApiError) throw e;
      throw new ApiError(text || res.statusText, res.status);
    }
  }
  if (!text) return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch {
    return text as unknown as T;
  }
}

export const api = {
  list: (cfg: ApiConfig) =>
    request<{ sandboxes: Sandbox[] }>(cfg, "/v1/sandboxes"),
  get: (cfg: ApiConfig, id: string) =>
    request<Sandbox>(cfg, `/v1/sandboxes/${id}`),
  create: (
    cfg: ApiConfig,
    image: string,
    opts: {
      exposedPort?: number;
      envVars?: Record<string, string>;
      cpuMillicores?: number;
      memoryBytes?: number;
    } = {},
  ) => {
    // Only include non-default fields so the controller's
    // "0 → DEFAULT_*" fallbacks stay in charge of platform defaults.
    // Sending 0 explicitly works today but leaks contract-internal
    // magic values into the wire.
    const body: Record<string, unknown> = { image };
    if (opts.exposedPort && opts.exposedPort > 0) {
      body.exposed_port = opts.exposedPort;
    }
    if (opts.envVars && Object.keys(opts.envVars).length > 0) {
      body.env_vars = opts.envVars;
    }
    if (opts.cpuMillicores && opts.cpuMillicores > 0) {
      body.cpu_millicores = opts.cpuMillicores;
    }
    if (opts.memoryBytes && opts.memoryBytes > 0) {
      body.memory_bytes = opts.memoryBytes;
    }
    return request<Sandbox>(cfg, "/v1/sandboxes", {
      method: "POST",
      body: JSON.stringify(body),
    });
  },
  remove: (cfg: ApiConfig, id: string) =>
    request<void>(cfg, `/v1/sandboxes/${id}`, { method: "DELETE" }),
  pause: (cfg: ApiConfig, id: string) =>
    request<{ status: string }>(cfg, `/v1/sandboxes/${id}/pause`, {
      method: "POST",
    }),
  unpause: (cfg: ApiConfig, id: string) =>
    request<{ status: string }>(cfg, `/v1/sandboxes/${id}/unpause`, {
      method: "POST",
    }),
  readFile: async (cfg: ApiConfig, id: string, path: string) => {
    const r = await safeFetch(
      trimBase(cfg.base) +
        `/v1/sandboxes/${id}/files/read?path=${encodeURIComponent(path)}`,
      { headers: { Authorization: `Bearer ${cfg.key}` } },
    );
    if (!r.ok) {
      const t = await r.text();
      throw new ApiError(t || r.statusText, r.status);
    }
    return new Uint8Array(await r.arrayBuffer());
  },
  writeFile: (cfg: ApiConfig, id: string, path: string, content: string) =>
    request<{ success: boolean }>(
      cfg,
      `/v1/sandboxes/${id}/files/write_file`,
      {
        method: "POST",
        body: JSON.stringify({ path, content }),
      },
    ),
};

/** Tiny shell-style argv splitter. Handles single+double quotes; empty
 *  quoted args (`sh -c ""`) emit an empty string. */
export function parseCommand(input: string): string[] {
  const out: string[] = [];
  let cur = "";
  let started = false;
  let quote: '"' | "'" | null = null;
  const flush = () => {
    if (started) {
      out.push(cur);
      cur = "";
      started = false;
    }
  };
  for (const c of input) {
    if (quote) {
      if (c === quote) quote = null;
      else cur += c;
    } else if (c === '"' || c === "'") {
      quote = c;
      started = true;
    } else if (/\s/.test(c)) {
      flush();
    } else {
      cur += c;
      started = true;
    }
  }
  flush();
  return out;
}
