import { cn } from "@/lib/cn";

const STATUS: Record<string, string> = {
  running: "border-ok/30 bg-ok/10 text-ok",
  creating: "border-warn/30 bg-warn/10 text-warn",
  pending: "border-warn/30 bg-warn/10 text-warn",
  starting: "border-warn/30 bg-warn/10 text-warn",
  stopped: "border-err/30 bg-err/10 text-err",
  failed: "border-err/30 bg-err/10 text-err",
  gone: "border-err/30 bg-err/10 text-err",
  pausing: "border-accent/30 bg-accent/10 text-accent",
  paused: "border-accent/40 bg-accent/15 text-accent",
  unpausing: "border-accent/30 bg-accent/10 text-accent",
};

export function StatusBadge({ status }: { status: string }) {
  return (
    <span
      className={cn(
        "inline-flex items-center rounded-full border px-2 py-px font-mono text-[9.5px] font-semibold uppercase tracking-wider",
        STATUS[status] ?? "border-border bg-surface-2 text-fg-muted",
      )}
    >
      {status}
    </span>
  );
}
