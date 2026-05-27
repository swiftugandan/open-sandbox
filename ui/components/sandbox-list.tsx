"use client";

import { useState, useTransition } from "react";
import { Pause, Play, Plus, RefreshCw, Trash2 } from "lucide-react";
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
    try {
      if (sb.status === "running") {
        await api.pause(config, sb.sandbox_id);
      } else if (sb.status === "paused") {
        await api.unpause(config, sb.sandbox_id);
      }
      onMutated();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const remove = async (id: string) => {
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
    try {
      await api.remove(config, id);
      onMutated();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
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
            <Plus className="size-3.5" />
            Create
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
          sandboxes.map((sb) => (
            <button
              key={sb.sandbox_id}
              onClick={() => onSelect(sb.sandbox_id)}
              className={cn(
                "group flex w-full items-center gap-2 border-l-2 border-transparent px-3 py-2.5 text-left transition-colors hover:bg-surface-2",
                sb.sandbox_id === selectedId &&
                  "border-l-accent bg-surface-2",
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
                  // Same hover-visibility behavior as Delete: always
                  // visible on touch, dimmed-until-hover at >=lg.
                  className="rounded p-1.5 text-fg-muted transition hover:bg-accent/20 hover:text-accent lg:p-1 lg:opacity-0 lg:group-hover:opacity-100"
                  title={sb.status === "running" ? "Pause" : "Resume"}
                >
                  {sb.status === "running" ? (
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
                // Always visible on touch (hover state doesn't exist there);
                // dimmed-until-hover only at >=lg.
                className="rounded p-1.5 text-fg-muted transition hover:bg-err/20 hover:text-err lg:p-1 lg:opacity-0 lg:group-hover:opacity-100"
                title="Delete"
              >
                <Trash2 className="size-3.5" />
              </button>
            </button>
          ))
        )}
      </div>
    </div>
  );
}
