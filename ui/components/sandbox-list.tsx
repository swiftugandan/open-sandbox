"use client";

import { useCallback, useState, useTransition } from "react";
import { Loader2, Pause, Play, Plus, RefreshCw, Trash2 } from "lucide-react";
import type { Sandbox, ApiConfig } from "@/lib/api";
import { api } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { StatusBadge } from "@/components/ui/badge";
import { useConfirm } from "@/components/ui/confirm-dialog";
import { cn } from "@/lib/cn";

interface Props {
  config: ApiConfig;
  sandboxes: Sandbox[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onMutated: () => void;
  refreshing: boolean;
}

export function SandboxList({
  config,
  sandboxes,
  selectedId,
  onSelect,
  onMutated,
  refreshing,
}: Props) {
  const [image, setImage] = useState("alpine:3.21");
  const [creating, startCreate] = useTransition();
  const [error, setError] = useState<string | null>(null);
  // In-flight per-row mutations: maps sandbox_id → the op currently
  // running. Used to swap action icons for a spinner, dim the row,
  // and dedupe rapid double-clicks (the 0–3s window between dispatch
  // and the next poll's status refresh).
  const [pending, setPending] = useState<
    Map<string, "pause" | "unpause" | "delete">
  >(new Map());
  const setPendingOp = useCallback(
    (id: string, op: "pause" | "unpause" | "delete" | null) => {
      setPending((prev) => {
        const next = new Map(prev);
        if (op === null) next.delete(id);
        else next.set(id, op);
        return next;
      });
    },
    [],
  );
  const confirm = useConfirm();

  const create = () => {
    setError(null);
    startCreate(async () => {
      try {
        const sb = await api.create(config, image.trim());
        onMutated();
        onSelect(sb.sandbox_id);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    });
  };

  const togglePause = async (sb: Sandbox) => {
    if (pending.has(sb.sandbox_id)) return;
    const op: "pause" | "unpause" | null =
      sb.status === "running"
        ? "pause"
        : sb.status === "paused"
          ? "unpause"
          : null;
    if (!op) return;
    setPendingOp(sb.sandbox_id, op);
    try {
      if (op === "pause") {
        await api.pause(config, sb.sandbox_id);
      } else {
        await api.unpause(config, sb.sandbox_id);
      }
      onMutated();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setPendingOp(sb.sandbox_id, null);
    }
  };

  const remove = async (id: string) => {
    if (pending.has(id)) return;
    const ok = await confirm({
      title: "Delete sandbox?",
      description: (
        <>
          <span className="font-mono">{id.slice(0, 8)}…{id.slice(-4)}</span>
          {" will be permanently destroyed and cannot be recovered."}
        </>
      ),
      confirmLabel: "Delete",
      variant: "danger",
    });
    if (!ok) return;
    setPendingOp(id, "delete");
    try {
      await api.remove(config, id);
      onMutated();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setPendingOp(id, null);
    }
  };

  return (
    <div className="flex h-full flex-col">
      <div className="space-y-2 border-b border-border p-3">
        <Input
          value={image}
          onChange={(e) => setImage(e.target.value)}
          placeholder="image (e.g. alpine:3.21)"
          onKeyDown={(e) => {
            if (e.key === "Enter") create();
          }}
          disabled={creating}
        />
        <div className="flex gap-2">
          <Button onClick={create} disabled={creating || !image.trim()}>
            {creating ? (
              <Loader2 className="size-3.5 animate-spin" />
            ) : (
              <Plus className="size-3.5" />
            )}
            {creating ? "Creating…" : "Create"}
          </Button>
          <Button variant="secondary" onClick={onMutated} disabled={refreshing}>
            <RefreshCw
              className={cn("size-3.5", refreshing && "animate-spin")}
            />
            Refresh
          </Button>
        </div>
        {error && (
          <div className="rounded-md border border-err/30 bg-err/10 px-2 py-1.5 text-[11px] text-err">
            {error}
          </div>
        )}
      </div>
      <div className="flex-1 overflow-y-auto py-1">
        {sandboxes.length === 0 ? (
          <div className="px-4 py-8 text-center text-[11.5px] text-fg-muted">
            No sandboxes yet — create one above.
          </div>
        ) : (
          sandboxes.map((sb) => {
            const inFlight = pending.get(sb.sandbox_id);
            const isDeleting = inFlight === "delete";
            const isToggling = inFlight === "pause" || inFlight === "unpause";
            return (
              // role=button + tabIndex so the row stays keyboard-
              // selectable, but rendered as a <div> because HTML forbids
              // nested interactive elements (the pause/delete <button>s
              // live inside this row). Keyboard handler mirrors the
              // implicit Enter/Space behavior a real <button> would
              // provide, but stopPropagation on the inner buttons keeps
              // them from also triggering selection.
              <div
                key={sb.sandbox_id}
                role="button"
                tabIndex={0}
                onClick={() => onSelect(sb.sandbox_id)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onSelect(sb.sandbox_id);
                  }
                }}
                className={cn(
                  "group flex w-full cursor-pointer items-center gap-2 border-l-2 border-transparent px-3 py-2.5 text-left transition-colors hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent/50",
                  sb.sandbox_id === selectedId &&
                    "border-l-accent bg-surface-2",
                  // Optimistic visual: dim the row while a destructive
                  // mutation is in flight so the user gets immediate
                  // confirmation without waiting for the next poll.
                  isDeleting && "opacity-50",
                )}
              >
                <div className="min-w-0 flex-1">
                  <div
                    className="truncate font-mono text-[11.5px] font-medium"
                    title={sb.sandbox_id}
                  >
                    {sb.sandbox_id.slice(0, 8)}…{sb.sandbox_id.slice(-4)}
                  </div>
                  <div className="truncate text-[10.5px] text-fg-muted">
                    agent {sb.agent_id.slice(0, 8)} · {sb.subdomain}
                  </div>
                </div>
                <StatusBadge status={sb.status} />
                {(sb.status === "running" || sb.status === "paused") && (
                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      togglePause(sb);
                    }}
                    disabled={isToggling || isDeleting}
                    // Pinned visible while busy so the spinner stays on
                    // screen (otherwise opacity-0 would hide it once the
                    // pointer leaves the row).
                    className={cn(
                      "rounded p-1.5 text-fg-muted transition hover:bg-accent/20 hover:text-accent lg:p-1",
                      isToggling
                        ? "lg:opacity-100"
                        : "lg:opacity-0 lg:group-hover:opacity-100",
                    )}
                    title={
                      isToggling
                        ? inFlight === "pause"
                          ? "pausing…"
                          : "resuming…"
                        : sb.status === "running"
                          ? "Pause"
                          : "Resume"
                    }
                  >
                    {isToggling ? (
                      <Loader2 className="size-3.5 animate-spin" />
                    ) : sb.status === "running" ? (
                      <Pause className="size-3.5" />
                    ) : (
                      <Play className="size-3.5" />
                    )}
                  </button>
                )}
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    remove(sb.sandbox_id);
                  }}
                  disabled={isDeleting || isToggling}
                  className={cn(
                    "rounded p-1.5 text-fg-muted transition hover:bg-err/20 hover:text-err lg:p-1",
                    isDeleting
                      ? "lg:opacity-100"
                      : "lg:opacity-0 lg:group-hover:opacity-100",
                  )}
                  title={isDeleting ? "deleting…" : "Delete"}
                >
                  {isDeleting ? (
                    <Loader2 className="size-3.5 animate-spin" />
                  ) : (
                    <Trash2 className="size-3.5" />
                  )}
                </button>
              </div>
            );
          })
        )}
      </div>
    </div>
  );
}
