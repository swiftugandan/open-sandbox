"use client";

import { useCallback, useEffect, useState } from "react";
import type { Sandbox } from "@/lib/api";
import { ApiError, api } from "@/lib/api";
import { useConfig } from "@/lib/config-store";
import { HeaderBar } from "@/components/header-bar";
import { SandboxList } from "@/components/sandbox-list";
import { RightPane } from "@/components/right-pane";
import { Drawer } from "@/components/ui/drawer";
import { ConfirmProvider } from "@/components/ui/confirm-dialog";

const POLL_MS = 3000;
const MOBILE_BREAKPOINT = 1024; // matches Tailwind `lg`

function useIsMobile() {
  const [isMobile, setIsMobile] = useState(false);
  useEffect(() => {
    const mq = window.matchMedia(`(max-width: ${MOBILE_BREAKPOINT - 1}px)`);
    const onChange = () => setIsMobile(mq.matches);
    onChange();
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  return isMobile;
}

export function Console() {
  const [config, setConfig, storageError] = useConfig();
  const [sandboxes, setSandboxes] = useState<Sandbox[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [connState, setConnState] = useState<
    "connected" | "connecting" | "error"
  >("connecting");
  const [detail, setDetail] = useState("connecting…");
  const [refreshing, setRefreshing] = useState(false);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const isMobile = useIsMobile();

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const data = await api.list(config);
      setSandboxes(data.sandboxes);
      setConnState("connected");
      setDetail(
        `connected · ${data.sandboxes.length} sandbox${data.sandboxes.length === 1 ? "" : "es"}`,
      );
    } catch (e) {
      setConnState("error");
      if (e instanceof ApiError && e.status === 401) {
        setDetail("unauthorized — check Bearer");
      } else if (e instanceof ApiError && e.status === 0) {
        // Transport-level failure (network, CORS, DNS, mixed-content,
        // api gateway not running). The wrapped ApiError message has
        // the actionable detail; render that, not "HTTP 0".
        setDetail(e.message);
      } else if (e instanceof ApiError) {
        setDetail(`HTTP ${e.status}`);
      } else {
        setDetail("unreachable");
      }
    } finally {
      setRefreshing(false);
    }
  }, [config]);

  useEffect(() => {
    void refresh();
    const t = setInterval(refresh, POLL_MS);
    return () => clearInterval(t);
  }, [refresh]);

  const handleSelect = useCallback((id: string) => {
    setSelectedId(id);
    setDrawerOpen(false);
  }, []);

  const selected =
    sandboxes.find((s) => s.sandbox_id === selectedId) ?? null;

  const list = (
    <SandboxList
      config={config}
      sandboxes={sandboxes}
      selectedId={selectedId}
      onSelect={handleSelect}
      onMutated={refresh}
      refreshing={refreshing}
    />
  );

  return (
    <ConfirmProvider>
    <div className="flex h-dvh flex-col bg-bg">
      <HeaderBar
        config={config}
        onChange={setConfig}
        connState={connState}
        detail={detail}
        storageError={storageError}
        onMenu={isMobile ? () => setDrawerOpen(true) : undefined}
      />
      <main className="grid min-h-0 flex-1 grid-cols-[minmax(0,1fr)] lg:grid-cols-[300px_minmax(0,1fr)]">
        <aside className="hidden min-h-0 border-r border-border bg-surface lg:block">
          {list}
        </aside>
        <section className="min-h-0 min-w-0">
          <RightPane
            config={config}
            sandbox={selected}
            onOpenList={isMobile ? () => setDrawerOpen(true) : undefined}
          />
        </section>
      </main>

      {isMobile && (
        <Drawer
          open={drawerOpen}
          onClose={() => setDrawerOpen(false)}
          side="left"
          title="Sandboxes"
        >
          {list}
        </Drawer>
      )}
    </div>
    </ConfirmProvider>
  );
}
