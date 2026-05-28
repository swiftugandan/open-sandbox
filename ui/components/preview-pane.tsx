"use client";

/** v1.0.3 live-edit preview pane (PLAN_LIVE_EDIT_TASKS D13).
 *
 *  Renders the sandbox's public URL inside an iframe. The parent
 *  (LiveEditPanel) drives reloads by bumping `reloadKey`; this
 *  component re-creates the iframe with a `?__t=<nonce>` cache-
 *  bust query so the browser actually fetches fresh content
 *  (cross-origin iframes don't expose `contentWindow.location.
 *  reload()` to us; the cache-bust + src swap is the portable
 *  fallback).
 *
 *  Empty state: when the sandbox isn't running (status != running)
 *  show a quiet "no preview" affordance instead of a broken
 *  iframe.
 */

import { useEffect, useMemo, useState } from "react";
import { Check, Copy, ExternalLink, RefreshCw } from "lucide-react";

import { isRunningStatus, type SandboxStatus } from "@/lib/api";
import { Button } from "@/components/ui/button";

interface Props {
  publicUrl: string;
  status: SandboxStatus;
  /** Incremented by the parent on save-chain completion to force
   *  a cache-busted reload. */
  reloadKey: number;
  /** Manual reload (toolbar button). */
  onManualReload: () => void;
  /** In-container port the iframe expects content on. Surfaced in
   *  the header so the user can see which port they're previewing
   *  — actual wire-call uses this via the parent's wait_port_
   *  listening call. */
  port: number;
}

export function PreviewPane({
  publicUrl,
  status,
  reloadKey,
  onManualReload,
  port,
}: Props) {
  const iframeSrc = useMemo(() => {
    // Append `__t=<nonce>` so a re-render with a new reloadKey
    // actually re-fetches. `?` vs `&` — keep it simple, the
    // sandbox URLs we generate don't carry existing query
    // strings.
    const sep = publicUrl.includes("?") ? "&" : "?";
    return `${publicUrl}${sep}__t=${reloadKey}`;
  }, [publicUrl, reloadKey]);

  // v1.0.3: copy the sandbox's public URL to the clipboard. This is
  // now the only in-UI copy affordance for the URL (the previous
  // top-of-pane URL bar was removed in commit 7208938); other tabs
  // surface the URL only via the sandbox-list / Info JSON. Clears
  // the "copied" indicator after 1.5s.
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const t = window.setTimeout(() => setCopied(false), 1500);
    return () => window.clearTimeout(t);
  }, [copied]);
  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(publicUrl);
      setCopied(true);
    } catch {
      // Fall back to a selection-based copy via a hidden
      // textarea — covers the not-focused / insecure-context
      // case clipboard.writeText rejects in.
      const ta = document.createElement("textarea");
      ta.value = publicUrl;
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
    <div className="flex flex-col h-full min-h-0">
      <div className="flex items-center gap-1.5 border-b border-border bg-surface-1 px-3 py-1.5 shrink-0">
        <span className="text-[10.5px] uppercase tracking-wider text-fg-muted shrink-0">
          Preview :{port}
        </span>
        <a
          href={publicUrl}
          target="_blank"
          rel="noreferrer noopener"
          className="group flex min-w-0 flex-1 items-center gap-1 truncate font-mono text-[11.5px] text-fg-muted transition-colors hover:text-accent"
          title={`Open ${publicUrl} in a new tab`}
        >
          <span className="truncate">{publicUrl}</span>
          <ExternalLink
            size={11}
            className="shrink-0 opacity-0 transition-opacity group-hover:opacity-100"
          />
        </a>
        <Button
          size="icon"
          variant="ghost"
          title={copied ? "Copied!" : "Copy URL"}
          onClick={onCopy}
          aria-label={copied ? "URL copied" : "Copy URL"}
        >
          {copied ? (
            <Check size={12} className="text-ok" />
          ) : (
            <Copy size={12} />
          )}
        </Button>
        <Button
          size="icon"
          variant="ghost"
          title="Reload preview"
          onClick={onManualReload}
        >
          <RefreshCw size={12} />
        </Button>
      </div>
      <div className="flex-1 min-h-0">
        {isRunningStatus(status) ? (
          <iframe
            // Force a fresh iframe (not just an src attribute
            // swap) on reloadKey change. Some browsers cache
            // aggressively even with the cache-bust query —
            // re-creating the element side-steps that entirely.
            key={reloadKey}
            src={iframeSrc}
            title="Sandbox preview"
            className="w-full h-full border-0 bg-white"
            // The sandbox is untrusted code by definition. The
            // public URL serves an arbitrary HTTP server inside
            // the sandbox container; allow-same-origin would
            // grant it access to the parent UI's cookies for
            // its OWN origin (the `<id>.localtest.me` host),
            // which is fine since that's isolated per-sandbox.
            // We explicitly DO NOT include allow-top-navigation
            // (so the sandbox can't escape the iframe).
            sandbox="allow-scripts allow-forms allow-same-origin allow-popups"
          />
        ) : (
          <PreviewEmptyState status={status} />
        )}
      </div>
    </div>
  );
}

function PreviewEmptyState({ status }: { status: SandboxStatus }) {
  const message =
    status === "creating"
      ? "Sandbox is still booting — preview will appear when it's running."
      : status === "stopped" || status === "stopping"
        ? "Sandbox is stopped. Restart it from the sandbox list."
        : status === "failed"
          ? "Sandbox failed to start. Check the Info tab for details."
          : `Sandbox is ${status}. Preview unavailable.`;
  return (
    <div className="flex h-full items-center justify-center p-6 text-center text-[12px] text-fg-muted">
      {message}
    </div>
  );
}
