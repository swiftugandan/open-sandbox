// REST client + WS auth helpers for the open-sandbox dev console.
// Both auth paths (Authorization header for REST, Sec-WebSocket-Protocol
// subprotocol for WS) are documented in CONTRACTS.md § WebSocket auth.

// Must match WS_AUTH_PROTOCOL_SENTINEL in crates/contracts/src/constants.rs.
export const WS_AUTH_SENTINEL = "open-sandbox.v1";

export interface Sandbox {
  sandbox_id: string;
  agent_id: string;
  subdomain: string;
  status: string;
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
  create: (cfg: ApiConfig, image: string) =>
    request<Sandbox>(cfg, "/v1/sandboxes", {
      method: "POST",
      body: JSON.stringify({ image }),
    }),
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
