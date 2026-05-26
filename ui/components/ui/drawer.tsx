"use client";

import * as React from "react";
import { X } from "lucide-react";
import { cn } from "@/lib/cn";

interface DrawerProps {
  open: boolean;
  onClose: () => void;
  side?: "left" | "right";
  title?: React.ReactNode;
  className?: string;
  children: React.ReactNode;
}

/** Off-canvas panel for narrow viewports. Backdrop click and ESC close it. */
export function Drawer({
  open,
  onClose,
  side = "left",
  title,
  className,
  children,
}: DrawerProps) {
  React.useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    document.body.style.overflow = "hidden";
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.style.overflow = "";
    };
  }, [open, onClose]);

  return (
    <>
      <div
        className={cn(
          "fixed inset-0 z-40 bg-black/60 backdrop-blur-sm transition-opacity",
          open ? "opacity-100" : "pointer-events-none opacity-0",
        )}
        onClick={onClose}
        aria-hidden
      />
      <aside
        className={cn(
          "fixed inset-y-0 z-50 flex w-[85vw] max-w-[320px] flex-col border-border bg-surface shadow-2xl transition-transform duration-200 ease-out",
          side === "left" ? "left-0 border-r" : "right-0 border-l",
          open
            ? "translate-x-0"
            : side === "left"
              ? "-translate-x-full"
              : "translate-x-full",
          className,
        )}
        role="dialog"
        aria-modal="true"
      >
        {title && (
          <header className="flex h-12 shrink-0 items-center justify-between border-b border-border px-3">
            <div className="text-[12px] font-semibold">{title}</div>
            <button
              onClick={onClose}
              className="rounded p-1 text-fg-muted hover:bg-surface-2 hover:text-fg"
              aria-label="Close"
            >
              <X className="size-4" />
            </button>
          </header>
        )}
        <div className="min-h-0 flex-1 overflow-hidden">{children}</div>
      </aside>
    </>
  );
}
