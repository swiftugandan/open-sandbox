"use client";

import { useEffect, useState } from "react";
import {
  Check,
  Copy,
  Edit3,
  ExternalLink,
  FileText,
  Info,
  ListChecks,
  Terminal,
} from "lucide-react";
import type { ApiConfig, Sandbox, SandboxStatus } from "@/lib/api";
import { isRunningStatus, publicUrl } from "@/lib/api";
import { ExecTerminal } from "@/components/exec-terminal";
import { FilesPanel } from "@/components/files-panel";
import { LiveEditPanel } from "@/components/live-edit-panel";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";

type Tab = "exec" | "edit" | "files" | "info";
const TABS: {
  id: Tab;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
}[] = [
  { id: "exec", label: "Exec", icon: Terminal },
  // v1.0.3 live-edit: tree + CodeMirror tabbed editor. Preview
  // iframe lands here too once D13 ships.
  { id: "edit", label: "Edit", icon: Edit3 },
  { id: "files", label: "Files", icon: FileText },
  { id: "info", label: "Info", icon: Info },
];

interface Props {
  config: ApiConfig;
  sandbox: Sandbox | null;
  /** Drives the no-selection empty state: with zero sandboxes
   *  anywhere, "Select a sandbox" is a lie — there's nothing to
   *  select. Swap to a "pick a template" pointer in that case. */
  hasAnySandboxes: boolean;
  /** Best-effort signal: did ExecTerminal observe a Started frame
   *  without a subsequent Exited? Drives the URL bar — false means
   *  hide the URL (nothing's bound to :8080). */
  urlExpected: boolean;
  /** Plumbed down to ExecTerminal so its WS lifecycle can update the
   *  Console-level urlExpected map. */
  onUrlExpectedChange: (id: string, on: boolean) => void;
  /** Provided on mobile: lets the empty/active panes reopen the sandbox list drawer. */
  onOpenList?: () => void;
}

export function RightPane({
  config,
  sandbox,
  hasAnySandboxes,
  urlExpected,
  onUrlExpectedChange,
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

  const url = publicUrl(config.base, sandbox.subdomain);
  const urlVisible = isRunningStatus(sandbox.status) && urlExpected;

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col">
      {/* v1.0.3: the URL bar is redundant on the Edit tab — the
       *  preview pane already renders the public URL with an
       *  open-in-tab affordance + a copy button. Hide it there
       *  so the Edit tab gets the full vertical space for the
       *  three-column layout. Other tabs (Exec / Files / Info)
       *  still surface the URL since they have no other
       *  on-screen reference to it. */}
      {tab !== "edit" && (
        <SandboxUrlBar
          url={url}
          visible={urlVisible}
          status={sandbox.status}
        />
      )}
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
            onUrlExpectedChange={onUrlExpectedChange}
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
         * the user is on Exec/Files/Info. Once mounted we
         * keep it mounted to preserve state between tab
         * switches.
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
        <div className={cn("h-full", tab !== "files" && "hidden")}>
          <FilesPanel config={config} sandboxId={sandbox.sandbox_id} />
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

function SandboxUrlBar({
  url,
  visible,
  status,
}: {
  url: string;
  visible: boolean;
  status: SandboxStatus;
}) {
  // When the URL would 502, show context instead. Three cases worth
  // distinguishing: not yet running (still booting), running but
  // nothing observed bound to :8080 (blank sandbox, exited process,
  // or just hasn't been started yet), and the happy path.
  if (!visible) {
    const message = !isRunningStatus(status)
      ? `URL appears once the sandbox is running (status: ${status})`
      : "Start a process on :8080 in the Exec tab to get a public URL";
    return (
      <div className="flex shrink-0 items-center gap-1.5 border-b border-border bg-surface px-3 py-1.5">
        <span className="shrink-0 text-[10.5px] uppercase tracking-wider text-fg-muted">
          URL
        </span>
        <span className="min-w-0 flex-1 truncate text-[11.5px] italic text-fg-muted">
          {message}
        </span>
      </div>
    );
  }
  return <SandboxUrlBarLive url={url} />;
}

function SandboxUrlBarLive({ url }: { url: string }) {
  const [copied, setCopied] = useState(false);
  // Auto-clear the "copied" indicator after a short window so the
  // button doesn't read as "copied" indefinitely after the user moves
  // on; cleanup cancels the timer if they click again first.
  useEffect(() => {
    if (!copied) return;
    const t = window.setTimeout(() => setCopied(false), 1500);
    return () => window.clearTimeout(t);
  }, [copied]);

  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
    } catch {
      // clipboard.writeText rejects when the page isn't focused / in
      // an insecure context; fall back to a selection-based copy via
      // a hidden textarea so the user still gets *something*.
      const ta = document.createElement("textarea");
      ta.value = url;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand("copy");
        setCopied(true);
      } catch {
        /* nothing left to try */
      }
      document.body.removeChild(ta);
    }
  };

  return (
    <div className="flex shrink-0 items-center gap-1.5 border-b border-border bg-surface px-3 py-1.5">
      <span className="shrink-0 text-[10.5px] uppercase tracking-wider text-fg-muted">
        URL
      </span>
      <a
        href={url}
        target="_blank"
        rel="noreferrer noopener"
        className="group flex min-w-0 flex-1 items-center gap-1 truncate font-mono text-[11.5px] text-fg-muted transition-colors hover:text-accent"
        title={`Open ${url} in a new tab`}
      >
        <span className="truncate">{url}</span>
        <ExternalLink className="size-3 shrink-0 opacity-0 transition-opacity group-hover:opacity-100" />
      </a>
      <button
        type="button"
        onClick={onCopy}
        title={copied ? "Copied!" : "Copy URL"}
        className={cn(
          "flex shrink-0 items-center gap-1 rounded px-1.5 py-1 text-[11px] transition-colors",
          copied
            ? "text-ok"
            : "text-fg-muted hover:bg-surface-2 hover:text-fg",
        )}
      >
        {copied ? (
          <>
            <Check className="size-3.5" />
            <span>copied</span>
          </>
        ) : (
          <>
            <Copy className="size-3.5" />
            <span className="hidden sm:inline">copy</span>
          </>
        )}
      </button>
    </div>
  );
}
