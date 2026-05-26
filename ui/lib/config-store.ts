"use client";

import { useEffect, useState } from "react";
import type { ApiConfig } from "./api";

const KEY = "open-sandbox-ui:config";

// On the server (SSR) we don't know the host yet — use a placeholder.
// The client-side lazy init below overrides this immediately on mount.
const SSR_FALLBACK: ApiConfig = {
  base: "http://127.0.0.1:8081",
  key: "dev-api-key",
};

function defaultApiBase(): string {
  // Derive from the page's own host so a LAN client browsing
  // http://<lan-ip>:8090 defaults to http://<lan-ip>:8081 without having
  // to type it. Falls back to the loopback default on SSR.
  if (typeof window === "undefined") return SSR_FALLBACK.base;
  return `${window.location.protocol}//${window.location.hostname}:8081`;
}

function loadInitial(): ApiConfig {
  if (typeof window === "undefined") return SSR_FALLBACK;
  const defaults: ApiConfig = { base: defaultApiBase(), key: "dev-api-key" };
  try {
    const raw = window.localStorage.getItem(KEY);
    if (raw) return { ...defaults, ...JSON.parse(raw) };
  } catch {
    /* ignore */
  }
  return defaults;
}

export function useConfig(): [ApiConfig, (next: ApiConfig) => void] {
  // Lazy init so DEFAULTS are picked up immediately on first render — no
  // separate "hydrated" gate is needed. On the server the lazy init still
  // returns DEFAULTS (typeof window === "undefined"); on the client it
  // reads localStorage synchronously.
  const [config, setConfig] = useState<ApiConfig>(loadInitial);

  const update = (next: ApiConfig) => {
    setConfig(next);
    try {
      window.localStorage.setItem(KEY, JSON.stringify(next));
    } catch {
      /* ignore */
    }
  };

  // Re-sync if another tab updates the config.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === KEY && e.newValue) {
        try {
          const next = JSON.parse(e.newValue);
          setConfig({ base: defaultApiBase(), key: "dev-api-key", ...next });
        } catch {
          /* ignore */
        }
      }
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  return [config, update];
}
