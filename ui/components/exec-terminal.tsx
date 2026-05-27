"use client";

import { useEffect, useRef, useState } from "react";
import { Play, Square, Plug, PlugZap } from "lucide-react";
import type { ApiConfig } from "@/lib/api";
import {
  WS_AUTH_SENTINEL,
  b64urlEncode,
  parseCommand,
  wsBase,
} from "@/lib/api";
import {
  Kind,
  decodeIoError,
  decodeIoExited,
  decodeIoStarted,
  encodeIoSignal,
  encodeIoStartExec,
  frame,
} from "@/lib/protobuf";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/cn";

type WsState = "idle" | "connecting" | "open" | "closed";

interface Props {
  config: ApiConfig;
  sandboxId: string;
}

export function ExecTerminal({ config, sandboxId }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<{
    term: import("@xterm/xterm").Terminal;
    fit: import("@xterm/addon-fit").FitAddon;
  } | null>(null);
  const wsRef = useRef<WebSocket | null>(null);

  const [cmd, setCmd] = useState('sh -c "uname -a && echo hello"');
  const [state, setState] = useState<WsState>("idle");

  // Init xterm once
  useEffect(() => {
    if (!containerRef.current || termRef.current) return;
    let mounted = true;
    (async () => {
      const [{ Terminal }, { FitAddon }] = await Promise.all([
        import("@xterm/xterm"),
        import("@xterm/addon-fit"),
      ]);
      if (!mounted || !containerRef.current) return;
      const term = new Terminal({
        fontFamily:
          'JetBrains Mono, ui-monospace, "SF Mono", Menlo, Consolas, monospace',
        fontSize: 12.5,
        lineHeight: 1.3,
        theme: {
          background: "#0b0d10",
          foreground: "#e6e9ef",
          cursor: "#7aa2ff",
          selectionBackground: "#7aa2ff44",
        },
        convertEol: true,
        cursorBlink: true,
        allowProposedApi: true,
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(containerRef.current);
      fit.fit();
      term.onData((data) => {
        const ws = wsRef.current;
        if (ws && ws.readyState === WebSocket.OPEN) {
          ws.send(frame(Kind.Stdin, new TextEncoder().encode(data)));
        }
      });
      termRef.current = { term, fit };
    })();

    return () => {
      mounted = false;
      wsRef.current?.close();
      termRef.current?.term.dispose();
      termRef.current = null;
    };
  }, []);

  // Fit on resize
  useEffect(() => {
    const onResize = () => termRef.current?.fit.fit();
    window.addEventListener("resize", onResize);
    const ro = new ResizeObserver(onResize);
    if (containerRef.current) ro.observe(containerRef.current);
    return () => {
      window.removeEventListener("resize", onResize);
      ro.disconnect();
    };
  }, []);

  // When the selected sandbox changes, close any open WS
  useEffect(() => {
    return () => {
      wsRef.current?.close();
    };
  }, [sandboxId]);

  const closeWS = () => {
    wsRef.current?.close();
    wsRef.current = null;
    setState("idle");
  };

  const run = () => {
    if (!termRef.current) return;
    if (wsRef.current) closeWS();
    const argv = parseCommand(cmd.trim());
    if (argv.length === 0) return;
    if (!config.key) {
      termRef.current.term.writeln(
        "\x1b[31mAPI key is empty — set one in the header.\x1b[0m",
      );
      return;
    }
    const { term } = termRef.current;
    term.writeln(`\x1b[38;5;245m$ ${argv.join(" ")}\x1b[0m`);

    const url = `${wsBase(config.base)}/v1/sandboxes/${sandboxId}/exec`;
    let ws: WebSocket;
    try {
      ws = new WebSocket(url, [
        WS_AUTH_SENTINEL,
        "bearer." + b64urlEncode(config.key),
      ]);
    } catch (e) {
      term.writeln(
        `\x1b[31mws ctor failed: ${e instanceof Error ? e.message : String(e)}\x1b[0m`,
      );
      return;
    }
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;
    setState("connecting");

    ws.onopen = () => {
      setState("open");
      const payload = encodeIoStartExec(sandboxId, argv);
      ws.send(frame(Kind.Start, payload));
    };
    ws.onmessage = (ev) => {
      const buf = new Uint8Array(ev.data as ArrayBuffer);
      if (buf.length < 1) return;
      const kind = buf[0];
      const body = buf.subarray(1);
      switch (kind) {
        case Kind.Started: {
          const s = decodeIoStarted(body);
          term.writeln(
            `\x1b[38;5;245mstarted exec_id=${s.execId} pid=${s.inContainerPid}\x1b[0m`,
          );
          break;
        }
        case Kind.Stdout:
          term.write(body);
          break;
        case Kind.Stderr:
          term.write("\x1b[31m");
          term.write(body);
          term.write("\x1b[0m");
          break;
        case Kind.Exited: {
          const e = decodeIoExited(body);
          const col = e.exitCode === 0 ? "32" : "31";
          const cnf = e.commandNotFound ? " (command not found)" : "";
          term.writeln(
            `\r\n\x1b[${col}mexited ${e.exitCode}${cnf}\x1b[0m`,
          );
          break;
        }
        case Kind.Error: {
          const e = decodeIoError(body);
          term.writeln(`\r\n\x1b[31m[${e.code}] ${e.detail}\x1b[0m`);
          break;
        }
        default:
          term.writeln(
            `\x1b[33m(unknown frame kind 0x${kind.toString(16)})\x1b[0m`,
          );
      }
    };
    ws.onclose = (ev) => {
      if (ev.code !== 1000 && ev.code !== 1005 && ev.reason) {
        term.writeln(
          `\x1b[38;5;245m(${ev.code} ${ev.reason})\x1b[0m`,
        );
      }
      wsRef.current = null;
      setState("closed");
    };
    ws.onerror = () => {
      setState("closed");
    };
  };

  const sigterm = () => {
    const ws = wsRef.current;
    const term = termRef.current?.term;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      term?.writeln(
        "\x1b[38;5;245m(no active session — nothing to signal)\x1b[0m",
      );
      return;
    }
    ws.send(frame(Kind.Signal, encodeIoSignal(15)));
    // Immediate user feedback: fire-and-forget signals otherwise
    // look identical to a dead button. The eventual "exited" line
    // confirms delivery; this line confirms intent.
    term?.writeln("\x1b[38;5;245m↳ sent SIGTERM (15)\x1b[0m");
  };

  const StateIcon = state === "open" ? PlugZap : Plug;
  const stateColor =
    state === "open"
      ? "text-ok"
      : state === "connecting"
        ? "text-warn"
        : "text-fg-muted";

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col">
      <div className="flex flex-wrap items-center gap-2 border-b border-border bg-surface px-3 py-2">
        <Input
          value={cmd}
          onChange={(e) => setCmd(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") run();
          }}
          placeholder='command (e.g. sh -c "uname -a; echo hello")'
          className="min-w-0 flex-1 basis-full sm:basis-0"
          // touch keyboards: don't autocorrect a shell line
          autoCapitalize="off"
          autoComplete="off"
          autoCorrect="off"
          spellCheck={false}
        />
        <Button onClick={run} disabled={state === "connecting"}>
          <Play className="size-3.5" />
          Run
        </Button>
        <Button
          variant="danger"
          onClick={sigterm}
          title="Send SIGTERM to the running process"
        >
          <Square className="size-3.5" />
        </Button>
        <div
          className={cn(
            "flex items-center gap-1 font-mono text-[11px]",
            stateColor,
          )}
        >
          <StateIcon className="size-3.5" />
          {state}
        </div>
      </div>
      <div
        ref={containerRef}
        className="min-h-0 min-w-0 flex-1 overflow-hidden bg-bg p-2"
      />
    </div>
  );
}
