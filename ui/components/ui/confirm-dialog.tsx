"use client";

import * as React from "react";
import { AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";

interface ConfirmOptions {
  title: string;
  description?: React.ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  variant?: "default" | "danger";
}

type Resolver = (value: boolean) => void;

interface ConfirmState extends ConfirmOptions {
  open: boolean;
  resolve: Resolver | null;
}

const ConfirmContext = React.createContext<
  ((opts: ConfirmOptions) => Promise<boolean>) | null
>(null);

/** Wraps the app so any descendant can call `useConfirm()` to get a
 *  Promise-returning replacement for window.confirm. Only one dialog can
 *  be open at a time; opening a second resolves the first with `false`. */
export function ConfirmProvider({ children }: { children: React.ReactNode }) {
  const [state, setState] = React.useState<ConfirmState>({
    open: false,
    title: "",
    resolve: null,
  });
  const confirmBtnRef = React.useRef<HTMLButtonElement>(null);

  const close = React.useCallback((result: boolean) => {
    setState((prev) => {
      prev.resolve?.(result);
      return { ...prev, open: false, resolve: null };
    });
  }, []);

  const confirm = React.useCallback(
    (opts: ConfirmOptions): Promise<boolean> => {
      return new Promise((resolve) => {
        setState((prev) => {
          // If a previous prompt is still pending, dismiss it.
          prev.resolve?.(false);
          return { ...opts, open: true, resolve };
        });
      });
    },
    [],
  );

  // ESC to cancel, Enter to confirm. Focus the confirm button on open.
  React.useEffect(() => {
    if (!state.open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close(false);
      } else if (e.key === "Enter") {
        e.preventDefault();
        close(true);
      }
    };
    window.addEventListener("keydown", onKey);
    document.body.style.overflow = "hidden";
    const t = window.setTimeout(() => confirmBtnRef.current?.focus(), 30);
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.style.overflow = "";
      window.clearTimeout(t);
    };
  }, [state.open, close]);

  const isDanger = state.variant === "danger";

  return (
    <ConfirmContext.Provider value={confirm}>
      {children}
      {/* Always-mounted backdrop + dialog so the open/close transition can play. */}
      <div
        className={cn(
          "fixed inset-0 z-[60] flex items-center justify-center bg-black/60 px-4 backdrop-blur-sm transition-opacity",
          state.open
            ? "opacity-100"
            : "pointer-events-none opacity-0",
        )}
        onClick={() => close(false)}
        aria-hidden={!state.open}
      >
        <div
          role="alertdialog"
          aria-modal="true"
          aria-labelledby="confirm-title"
          aria-describedby={state.description ? "confirm-desc" : undefined}
          className={cn(
            "w-full max-w-[400px] rounded-lg border border-border bg-surface shadow-2xl transition-transform",
            state.open ? "scale-100" : "scale-95",
          )}
          onClick={(e) => e.stopPropagation()}
        >
          <div className="flex gap-3 p-5">
            <div
              className={cn(
                "flex size-9 shrink-0 items-center justify-center rounded-full",
                isDanger ? "bg-err/15 text-err" : "bg-accent/15 text-accent",
              )}
            >
              <AlertTriangle className="size-4" />
            </div>
            <div className="flex-1 space-y-1">
              <h2
                id="confirm-title"
                className="text-[13.5px] font-semibold leading-tight"
              >
                {state.title}
              </h2>
              {state.description && (
                <div
                  id="confirm-desc"
                  className="text-[12px] leading-relaxed text-fg-muted"
                >
                  {state.description}
                </div>
              )}
            </div>
          </div>
          <div className="flex justify-end gap-2 border-t border-border bg-surface-2/60 px-5 py-3">
            <Button variant="secondary" onClick={() => close(false)}>
              {state.cancelLabel ?? "Cancel"}
            </Button>
            <Button
              ref={confirmBtnRef}
              variant={isDanger ? "danger" : "primary"}
              onClick={() => close(true)}
            >
              {state.confirmLabel ?? "OK"}
            </Button>
          </div>
        </div>
      </div>
    </ConfirmContext.Provider>
  );
}

export function useConfirm() {
  const ctx = React.useContext(ConfirmContext);
  if (!ctx) {
    throw new Error("useConfirm must be used inside <ConfirmProvider>");
  }
  return ctx;
}
