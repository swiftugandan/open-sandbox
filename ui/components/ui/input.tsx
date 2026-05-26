"use client";

import * as React from "react";
import { cn } from "@/lib/cn";

export const Input = React.forwardRef<
  HTMLInputElement,
  React.InputHTMLAttributes<HTMLInputElement>
>(({ className, ...props }, ref) => (
  <input
    ref={ref}
    className={cn(
      "h-8 w-full rounded-md border border-border bg-bg px-2.5 font-mono text-[12px] outline-none transition-colors placeholder:text-fg-muted focus:border-accent/60",
      className,
    )}
    {...props}
  />
));
Input.displayName = "Input";

export const Textarea = React.forwardRef<
  HTMLTextAreaElement,
  React.TextareaHTMLAttributes<HTMLTextAreaElement>
>(({ className, ...props }, ref) => (
  <textarea
    ref={ref}
    className={cn(
      "min-h-[120px] w-full rounded-md border border-border bg-bg px-3 py-2 font-mono text-[12px] outline-none transition-colors placeholder:text-fg-muted focus:border-accent/60",
      className,
    )}
    {...props}
  />
));
Textarea.displayName = "Textarea";
