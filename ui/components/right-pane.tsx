"use client";

import { useState } from "react";
import { Terminal, FileText, Info, ListChecks } from "lucide-react";
import type { ApiConfig, Sandbox } from "@/lib/api";
import { ExecTerminal } from "@/components/exec-terminal";
import { FilesPanel } from "@/components/files-panel";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";

type Tab = "exec" | "files" | "info";
const TABS: {
  id: Tab;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
}[] = [
  { id: "exec", label: "Exec", icon: Terminal },
  { id: "files", label: "Files", icon: FileText },
  { id: "info", label: "Info", icon: Info },
];

interface Props {
  config: ApiConfig;
  sandbox: Sandbox | null;
  /** Provided on mobile: lets the empty/active panes reopen the sandbox list drawer. */
  onOpenList?: () => void;
}

export function RightPane({ config, sandbox, onOpenList }: Props) {
  const [tab, setTab] = useState<Tab>("exec");

  if (!sandbox) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center text-[12.5px] text-fg-muted">
        <p>Select a sandbox to start, or create one from the list.</p>
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
          />
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
