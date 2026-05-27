"use client";

import { useEffect, useState } from "react";
import {
  AlertTriangle,
  Boxes,
  Eye,
  EyeOff,
  Menu,
  Settings,
} from "lucide-react";
import type { ApiConfig } from "@/lib/api";
import { Input } from "@/components/ui/input";
import { Drawer } from "@/components/ui/drawer";
import { cn } from "@/lib/cn";

interface Props {
  config: ApiConfig;
  onChange: (next: ApiConfig) => void;
  connState: "connected" | "connecting" | "error";
  detail: string;
  /** When non-null, localStorage.setItem failed (private mode, disabled
   *  storage). Surfaced in the settings drawer so the user knows their
   *  config won't persist across reloads. */
  storageError?: string | null;
  onMenu?: () => void;
}

// Idle window after the last keystroke before we propagate config
// edits up to the parent. Without this, every keystroke triggers a
// poll cycle against the partial value and the connection-state dot
// flashes connecting → error → connecting → ... while the user types.
const COMMIT_DEBOUNCE_MS = 300;

export function HeaderBar({
  config,
  onChange,
  connState,
  detail,
  storageError,
  onMenu,
}: Props) {
  const [settingsOpen, setSettingsOpen] = useState(false);

  const dotColor =
    connState === "connected"
      ? "bg-ok"
      : connState === "connecting"
        ? "bg-warn"
        : "bg-err";

  return (
    <>
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border bg-surface px-3 sm:gap-3 sm:px-4">
        {/* Hamburger — mobile only */}
        {onMenu && (
          <button
            onClick={onMenu}
            className="flex size-8 items-center justify-center rounded-md text-fg-muted hover:bg-surface-2 hover:text-fg lg:hidden"
            aria-label="Open sandbox list"
          >
            <Menu className="size-4" />
          </button>
        )}

        <div className="flex items-center gap-2">
          <Boxes className="size-4 text-accent" />
          <span className="font-semibold tracking-tight">open-sandbox</span>
          <span className="hidden text-[11px] text-fg-muted sm:inline">
            dev console
          </span>
        </div>

        {/* Inline API config — desktop only */}
        <div className="ml-2 hidden items-center gap-3 lg:flex">
          <span className="text-[11px] text-fg-muted">API</span>
          <DebouncedInput
            value={config.base}
            onCommit={(v) => onChange({ ...config, base: v })}
            className="w-60"
            spellCheck={false}
          />
          <span className="text-[11px] text-fg-muted">Bearer</span>
          <BearerInput config={config} onChange={onChange} />
        </div>

        {/* Connection state — always visible, condenses on mobile */}
        <div className="ml-auto flex items-center gap-2">
          <span className={cn("inline-block size-2 rounded-full", dotColor)} />
          <span className="hidden text-[11px] text-fg-muted sm:inline">
            {detail}
          </span>
        </div>

        {/* Settings button — opens sheet on mobile/tablet */}
        <button
          onClick={() => setSettingsOpen(true)}
          className="flex size-8 items-center justify-center rounded-md text-fg-muted hover:bg-surface-2 hover:text-fg lg:hidden"
          aria-label="API settings"
        >
          <Settings className="size-4" />
        </button>
      </header>

      <Drawer
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        side="right"
        title="API settings"
      >
        <div className="space-y-4 p-4">
          <div className="space-y-1.5">
            <label className="text-[11px] font-medium uppercase tracking-wider text-fg-muted">
              API base
            </label>
            <DebouncedInput
              value={config.base}
              onCommit={(v) => onChange({ ...config, base: v })}
              spellCheck={false}
            />
          </div>
          <div className="space-y-1.5">
            <label className="text-[11px] font-medium uppercase tracking-wider text-fg-muted">
              Bearer token
            </label>
            <BearerInput config={config} onChange={onChange} expand />
          </div>
          <div className="space-y-1.5">
            <div className="text-[11px] font-medium uppercase tracking-wider text-fg-muted">
              Status
            </div>
            <div className="flex items-center gap-2 rounded-md border border-border bg-bg px-3 py-2 text-[12px]">
              <span
                className={cn("inline-block size-2 rounded-full", dotColor)}
              />
              <span className="text-fg-muted">{detail}</span>
            </div>
          </div>
          {storageError && (
            <div className="flex items-start gap-2 rounded-md border border-warn/40 bg-warn/10 px-3 py-2 text-[11.5px] text-warn">
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
              <div>
                <div className="font-medium">Settings won't persist</div>
                <div className="text-[11px] opacity-80">
                  Browser storage is unavailable (private mode? disabled?).
                  Edits work for this tab but are lost on reload.
                  <span className="ml-1 font-mono opacity-70">
                    ({storageError})
                  </span>
                </div>
              </div>
            </div>
          )}
        </div>
      </Drawer>
    </>
  );
}

function BearerInput({
  config,
  onChange,
  expand = false,
}: {
  config: ApiConfig;
  onChange: (next: ApiConfig) => void;
  expand?: boolean;
}) {
  const [show, setShow] = useState(false);
  return (
    <div className={cn("relative", !expand && "w-48")}>
      <DebouncedInput
        type={show ? "text" : "password"}
        value={config.key}
        onCommit={(v) => onChange({ ...config, key: v })}
        className={cn("pr-7", expand && "w-full")}
        spellCheck={false}
      />
      <button
        type="button"
        onClick={() => setShow((s) => !s)}
        className="absolute right-1.5 top-1/2 -translate-y-1/2 rounded p-0.5 text-fg-muted hover:text-fg"
        aria-label={show ? "Hide token" : "Show token"}
      >
        {show ? (
          <EyeOff className="size-3.5" />
        ) : (
          <Eye className="size-3.5" />
        )}
      </button>
    </div>
  );
}

/** Input that keeps a local "draft" string and only calls `onCommit`
 *  after COMMIT_DEBOUNCE_MS of typing inactivity. Resyncs the draft
 *  if the parent value changes externally (cross-tab `storage` event).
 *  Used for config fields whose every-keystroke commit triggers a
 *  network poll downstream. */
function DebouncedInput({
  value,
  onCommit,
  ...rest
}: Omit<React.InputHTMLAttributes<HTMLInputElement>, "value" | "onChange"> & {
  value: string;
  onCommit: (next: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  // External value change (e.g. another tab updated localStorage) → sync.
  useEffect(() => {
    setDraft(value);
  }, [value]);
  // Debounced commit. Re-runs on every keystroke; the cleanup cancels
  // the previous timer so only the final value after the idle window
  // is propagated up.
  useEffect(() => {
    if (draft === value) return;
    const t = setTimeout(() => onCommit(draft), COMMIT_DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [draft, value, onCommit]);
  return (
    <Input
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      {...rest}
    />
  );
}
