"use client";

import { useState } from "react";
import { Download, Upload, Check, AlertCircle } from "lucide-react";
import type { ApiConfig } from "@/lib/api";
import { ApiError, api } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input, Textarea } from "@/components/ui/input";
import { cn } from "@/lib/cn";

interface Props {
  config: ApiConfig;
  sandboxId: string;
}

interface OutState {
  text: string;
  kind: "idle" | "info" | "ok" | "err";
}

export function FilesPanel({ config, sandboxId }: Props) {
  const [readPath, setReadPath] = useState("/etc/os-release");
  const [readOut, setReadOut] = useState<OutState>({
    text: "(no read yet)",
    kind: "idle",
  });
  const [writePath, setWritePath] = useState("/tmp/hello.txt");
  const [writeContent, setWriteContent] = useState(
    "hello from the dev console\n",
  );
  const [writeOut, setWriteOut] = useState<OutState>({
    text: "(no write yet)",
    kind: "idle",
  });
  const [readBusy, setReadBusy] = useState(false);
  const [writeBusy, setWriteBusy] = useState(false);

  const doRead = async () => {
    setReadBusy(true);
    setReadOut({ text: "reading…", kind: "info" });
    try {
      const buf = await api.readFile(config, sandboxId, readPath);
      let text: string;
      try {
        text = new TextDecoder("utf-8", { fatal: false }).decode(buf);
      } catch {
        text = `(${buf.length} bytes binary)`;
      }
      setReadOut({ text: text || "(empty file)", kind: "ok" });
    } catch (e) {
      const msg =
        e instanceof ApiError
          ? `HTTP ${e.status}\n${e.message}`
          : e instanceof Error
            ? e.message
            : String(e);
      setReadOut({ text: msg, kind: "err" });
    } finally {
      setReadBusy(false);
    }
  };

  const doWrite = async () => {
    setWriteBusy(true);
    setWriteOut({ text: "writing…", kind: "info" });
    try {
      const res = await api.writeFile(
        config,
        sandboxId,
        writePath,
        writeContent,
      );
      setWriteOut({ text: JSON.stringify(res), kind: "ok" });
    } catch (e) {
      const msg =
        e instanceof ApiError
          ? `HTTP ${e.status}\n${e.message}`
          : e instanceof Error
            ? e.message
            : String(e);
      setWriteOut({ text: msg, kind: "err" });
    } finally {
      setWriteBusy(false);
    }
  };

  return (
    <div className="grid h-full min-h-0 grid-cols-1 gap-4 overflow-y-auto p-4 xl:grid-cols-2">
      <Card title="Read file" icon={<Download className="size-3.5" />}>
        <div className="flex flex-wrap gap-2">
          <Input
            value={readPath}
            onChange={(e) => setReadPath(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") doRead();
            }}
            className="min-w-0 flex-1 basis-full sm:basis-0"
            autoCapitalize="off"
            autoComplete="off"
            autoCorrect="off"
            spellCheck={false}
          />
          <Button
            onClick={doRead}
            disabled={readBusy || !readPath}
            className="sm:ml-auto"
          >
            Read
          </Button>
        </div>
        <Out state={readOut} />
      </Card>

      <Card title="Write file" icon={<Upload className="size-3.5" />}>
        <Input
          value={writePath}
          onChange={(e) => setWritePath(e.target.value)}
          placeholder="/tmp/hello.txt"
        />
        <Textarea
          value={writeContent}
          onChange={(e) => setWriteContent(e.target.value)}
          placeholder="file contents…"
        />
        <div className="flex justify-end">
          <Button onClick={doWrite} disabled={writeBusy || !writePath}>
            Write
          </Button>
        </div>
        <Out state={writeOut} />
      </Card>
    </div>
  );
}

function Card({
  title,
  icon,
  children,
}: {
  title: string;
  icon: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="flex min-w-0 flex-col gap-2.5 rounded-lg border border-border bg-surface p-4">
      <div className="flex items-center gap-1.5 text-[12.5px] font-semibold">
        {icon}
        {title}
      </div>
      {children}
    </div>
  );
}

function Out({ state }: { state: OutState }) {
  if (state.kind === "idle") {
    return (
      <pre className="max-h-60 overflow-auto rounded-md border border-border bg-bg p-2.5 font-mono text-[11.5px] italic text-fg-muted">
        {state.text}
      </pre>
    );
  }
  const Icon = state.kind === "err" ? AlertCircle : Check;
  return (
    <div className="space-y-1.5">
      <div
        className={cn(
          "flex items-center gap-1 text-[11px]",
          state.kind === "err"
            ? "text-err"
            : state.kind === "ok"
              ? "text-ok"
              : "text-fg-muted",
        )}
      >
        <Icon className="size-3" />
        {state.kind === "err"
          ? "error"
          : state.kind === "ok"
            ? "success"
            : "in progress"}
      </div>
      <pre className="max-h-60 overflow-auto whitespace-pre-wrap rounded-md border border-border bg-bg p-2.5 font-mono text-[11.5px]">
        {state.text}
      </pre>
    </div>
  );
}
