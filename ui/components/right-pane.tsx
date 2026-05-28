"use client";

import { useEffect, useState } from "react";
import { Edit3, Info, ListChecks, Terminal } from "lucide-react";
import type { ApiConfig, Sandbox } from "@/lib/api";
import { ExecTerminal } from "@/components/exec-terminal";
import { LiveEditPanel } from "@/components/live-edit-panel";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";

type Tab = "exec" | "edit" | "info";
const TABS: {
  id: Tab;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
}[] = [
  { id: "exec", label: "Exec", icon: Terminal },
  // v1.0.3 live-edit: tree + CodeMirror tabbed editor + preview iframe.
  { id: "edit", label: "Edit", icon: Edit3 },
  { id: "info", label: "Info", icon: Info },
];

interface Props {
  config: ApiConfig;
  sandbox: Sandbox | null;
  /** Drives the no-selection empty state: with zero sandboxes
   *  anywhere, "Select a sandbox" is a lie — there's nothing to
   *  select. Swap to a "pick a template" pointer in that case. */
  hasAnySandboxes: boolean;
  /** Provided on mobile: lets the empty/active panes reopen the sandbox list drawer. */
  onOpenList?: () => void;
}

export function RightPane({
  config,
  sandbox,
  hasAnySandboxes,
  onOpenList,
}: Props) {
  const [tab, setTab] = useState<Tab>("exec");
  // v1.0.3 live-edit: once the user opens the Edit tab we keep
  // LiveEditPanel mounted (so tab/dirty state survives switching
  // away and back), but we skip mounting it until first visit.
  // Without this, every save-chain iframe reload fires a network
  // request against the sandbox's public URL even when the user
  // is on a different tab. Reset on sandbox change so a fresh
  // sandbox starts with no Edit-tab side-effects.
  const [editTabEverVisited, setEditTabEverVisited] = useState(false);
  useEffect(() => {
    setEditTabEverVisited(false);
  }, [sandbox?.sandbox_id]);
  useEffect(() => {
    if (tab === "edit") setEditTabEverVisited(true);
  }, [tab]);

  if (!sandbox) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center text-[12.5px] text-fg-muted">
        <p>
          {hasAnySandboxes
            ? "Select a sandbox from the list to see its terminal, files, and URL here."
            : "Configure a sandbox on the left — pick a template (or create a blank one), set env vars or resources if you need them, then hit Create & run."}
        </p>
        {onOpenList && (
          <Button onClick={onOpenList} variant="secondary">
            <ListChecks className="size-3.5" />
            Open sandbox list
          </Button>
        )}
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col">
      <nav className="flex shrink-0 items-stretch overflow-x-auto border-b border-border bg-surface">
        {TABS.map(({ id, label, icon: Icon }) => (
          <button
            key={id}
            onClick={() => setTab(id)}
            className={cn(
              "flex items-center justify-center gap-1.5 border-b-2 px-4 py-2.5 text-[12px] font-medium transition-colors",
              tab === id
                ? "border-accent text-fg"
                : "border-transparent text-fg-muted hover:text-fg",
            )}
          >
            <Icon className="size-3.5" />
            <span>{label}</span>
          </button>
        ))}
      </nav>
      <div className="min-h-0 min-w-0 flex-1">
        <div className={cn("h-full", tab !== "exec" && "hidden")}>
          <ExecTerminal
            key={sandbox.sandbox_id}
            config={config}
            sandboxId={sandbox.sandbox_id}
            sandboxStatus={sandbox.status}
          />
        </div>
        {/*
         * v1.0.3: key by sandbox_id so switching sandboxes
         * fully remounts the panel. Without this, the tab list,
         * dirty buffers, cached revisions, and reloadKey survive
         * a sandbox switch — and the now-stale openFile/onSave
         * closures fire writeFile against the wrong sandbox.
         * Matches ExecTerminal's keying convention above.
         *
         * Lazy-mount: only mount LiveEditPanel after the user
         * has visited the Edit tab at least once. The legacy
         * `className="hidden"` toggle keeps the panel rendered
         * while inactive, which keeps the iframe live and
         * fires real network requests against the sandbox's
         * public URL on every save-chain reload — even when
         * the user is on Exec/Info. Once mounted we keep it
         * mounted to preserve state between tab switches.
         */}
        <div className={cn("h-full", tab !== "edit" && "hidden")}>
          {editTabEverVisited && (
            <LiveEditPanel
              key={sandbox.sandbox_id}
              config={config}
              sandbox={sandbox}
            />
          )}
        </div>
        <div
          className={cn(
            "h-full overflow-y-auto p-4",
            tab !== "info" && "hidden",
          )}
        >
          <pre className="overflow-auto rounded-md border border-border bg-surface p-3 font-mono text-[12px]">
            {JSON.stringify(sandbox, null, 2)}
          </pre>
        </div>
      </div>
    </div>
  );
}
