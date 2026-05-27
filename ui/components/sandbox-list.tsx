"use client";

import { useCallback, useMemo, useState, useTransition } from "react";
import {
  ChevronDown,
  Loader2,
  Pause,
  Play,
  Plus,
  RefreshCw,
  Trash2,
} from "lucide-react";
import type { Sandbox, ApiConfig } from "@/lib/api";
import { api } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input, Textarea } from "@/components/ui/input";
import { StatusBadge } from "@/components/ui/badge";
import { useConfirm } from "@/components/ui/confirm-dialog";
import { cn } from "@/lib/cn";
import {
  BLANK_TEMPLATE_ID,
  DEFAULT_TEMPLATE_ID,
  DROPDOWN_TEMPLATES,
  findTemplate,
} from "@/lib/templates";
import { stashExecPrefill } from "@/lib/exec-prefill";
import { parseEnvVars } from "@/lib/env-vars";
import {
  DEFAULT_RESOURCE_TIER_ID,
  RESOURCE_TIERS,
  findTier,
} from "@/lib/resources";

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
  const [templateId, setTemplateId] = useState<string>(DEFAULT_TEMPLATE_ID);
  const [image, setImage] = useState(
    () => findTemplate(DEFAULT_TEMPLATE_ID)?.image ?? "alpine:3.21",
  );
  // Stringified so the input doesn't reformat 8080 → "8,080" on locales
  // and so we can render an empty input cleanly when the template
  // doesn't expose a port (the blank shell). Coerced to a number on
  // submit; invalid values fall back to the platform default.
  const [port, setPort] = useState<string>(
    () => String(findTemplate(DEFAULT_TEMPLATE_ID)?.exposedPort ?? 8080),
  );
  const [creating, startCreate] = useTransition();
  const [error, setError] = useState<string | null>(null);
  // Track which fields the user has hand-edited. Both fields autofill
  // from the template; once dirtied we stop overwriting them on
  // template change. `imageDirty` also suppresses autorun — a custom
  // image is likelier to be missing the template's binaries.
  const [imageDirty, setImageDirty] = useState(false);
  const [portDirty, setPortDirty] = useState(false);
  // Free-text textarea (KEY=value per line). Parsed on submit;
  // displayed count and any invalid lines come from useMemo below.
  const [envText, setEnvText] = useState("");
  const envParsed = useMemo(() => parseEnvVars(envText), [envText]);
  // S/M/L tier picker. Defaults to the platform-default tier so we
  // send no override and the controller stays in charge of defaults.
  const [tierId, setTierId] = useState<string>(DEFAULT_RESOURCE_TIER_ID);
  const tier = findTier(tierId);
  // Env vars is the one remaining collapsible — optional, often
  // empty, and a multi-line textarea would dominate the form visually
  // if always visible. Resources used to be a disclosure too but was
  // promoted to the inline top-of-form because hardware sits at the
  // top of the infra mental model (hardware → image → app).
  const [envOpen, setEnvOpen] = useState(false);
  const onTemplateChange = (id: string) => {
    setTemplateId(id);
    const t = findTemplate(id);
    if (!t) return;
    if (!imageDirty) setImage(t.image);
    if (!portDirty) setPort(String(t.exposedPort || 8080));
  };

  const tpl = findTemplate(templateId);
  // "Create & run" honestly advertises the autorun path: a template
  // with an exec, on its stock image. Anything else falls back to a
  // plain "Create" so we don't promise behavior we won't deliver.
  const willAutorun = Boolean(tpl?.execCommand) && !imageDirty;
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

  const createWith = (
    chosenTemplateId: string,
    chosenImage: string,
    chosenPort: number,
  ) => {
    setError(null);
    const t = findTemplate(chosenTemplateId);
    const isBlank = chosenTemplateId === BLANK_TEMPLATE_ID;
    const autorun = !isBlank && Boolean(t?.execCommand) && !imageDirty;
    // Resource tier: when the platform-default tier is selected, we
    // send no cpu/memory override so the controller's defaults stay
    // canonical (the user implicitly opts into "whatever Medium
    // means today, even if we tune it tomorrow").
    const tierForRequest = tier && !tier.isPlatformDefault ? tier : null;
    startCreate(async () => {
      try {
        const sb = await api.create(config, chosenImage.trim(), {
          exposedPort: chosenPort,
          envVars: envParsed.vars,
          cpuMillicores: tierForRequest?.cpuMillicores,
          memoryBytes: tierForRequest?.memoryBytes,
        });
        if (t?.execCommand) {
          stashExecPrefill(sb.sandbox_id, t.execCommand, { autorun });
        }
        onMutated();
        onSelect(sb.sandbox_id);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    });
  };
  // Empty is fine (= "use the platform default port"); a non-empty
  // value must parse to a valid TCP port. Earlier this code silently
  // coerced invalid input to 0 → controller default, so a user typing
  // "abc" would unknowingly land on 8080. Now we surface the error
  // and block Create until they fix it.
  const portTrimmed = port.trim();
  const portIsEmpty = portTrimmed === "";
  const portParsed = portIsEmpty ? 0 : Number(portTrimmed);
  const portValid =
    portIsEmpty ||
    (Number.isInteger(portParsed) && portParsed > 0 && portParsed < 65536);
  const create = () => createWith(templateId, image, portParsed);
  const createBlank = () => {
    const blank = findTemplate(BLANK_TEMPLATE_ID);
    if (!blank) return;
    createWith(BLANK_TEMPLATE_ID, blank.image, blank.exposedPort);
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
        <p className="text-[11px] leading-snug text-fg-muted">
          Spin up an isolated container that serves a public URL.
          Picking an <span className="text-fg">application</span>{" "}
          fills its port and command for you; everything else is
          editable.
        </p>
        {/* Order follows the infra mental model: hardware → image →
            image-level settings (port + env) → application (preset).
            Resources is inline (compact 3-up) rather than hidden in a
            disclosure because it sits at the top of the hierarchy;
            env-vars stay collapsed because they're optional and
            often empty. */}
        <div className="space-y-1">
          <label className="text-[10px] font-medium uppercase tracking-wider text-fg-muted">
            Resources
          </label>
          <div
            role="radiogroup"
            aria-label="Resource tier"
            className="grid grid-cols-3 gap-1 rounded-md border border-border bg-bg p-0.5"
          >
            {RESOURCE_TIERS.map((t) => (
              <button
                key={t.id}
                type="button"
                role="radio"
                aria-checked={tierId === t.id}
                onClick={() => setTierId(t.id)}
                disabled={creating}
                className={cn(
                  "rounded px-2 py-1 text-[11.5px] transition-colors disabled:opacity-50",
                  tierId === t.id
                    ? "bg-accent/20 text-fg"
                    : "text-fg-muted hover:bg-surface-2 hover:text-fg",
                )}
              >
                {t.label}
              </button>
            ))}
          </div>
          {tier && (
            <p className="text-[10px] leading-snug text-fg-muted">
              {tier.description}
            </p>
          )}
        </div>
        <div className="space-y-1">
          <label
            htmlFor="image-input"
            className="text-[10px] font-medium uppercase tracking-wider text-fg-muted"
          >
            Image
          </label>
          <Input
            id="image-input"
            value={image}
            onChange={(e) => {
              setImage(e.target.value);
              setImageDirty(true);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") create();
            }}
            disabled={creating}
            aria-label="Container image"
          />
          {imageDirty && (
            <p className="text-[10px] leading-snug text-fg-muted">
              Custom image disables auto-run. Must be a public image.
            </p>
          )}
        </div>
        <div className="space-y-1">
          <label className="text-[10px] font-medium uppercase tracking-wider text-fg-muted">
            Application
          </label>
          {/* Application = the workload that runs on top of the
              image. Sits ABOVE the app properties (port, env vars)
              because those properties belong to the application, not
              to the image: the app decides which port it binds and
              which env vars it reads. The 2-col chip grid keeps the
              widths uniform in the narrow sidebar. */}
          <div
            role="radiogroup"
            aria-label="Application preset"
            className="grid grid-cols-2 gap-1.5"
          >
            {DROPDOWN_TEMPLATES.map((t) => {
              const active = templateId === t.id;
              return (
                <button
                  key={t.id}
                  type="button"
                  role="radio"
                  aria-checked={active}
                  onClick={() => onTemplateChange(t.id)}
                  disabled={creating}
                  title={t.description}
                  className={cn(
                    "rounded-md border px-2.5 py-1.5 text-left text-[11.5px] transition-colors disabled:opacity-50",
                    active
                      ? "border-accent/60 bg-accent/10 text-fg"
                      : "border-border text-fg-muted hover:border-border-strong hover:text-fg",
                  )}
                >
                  {t.label}
                </button>
              );
            })}
          </div>
        </div>
        <div className="space-y-1">
          <label
            htmlFor="port-input"
            className="text-[10px] font-medium uppercase tracking-wider text-fg-muted"
          >
            Port
          </label>
          <Input
            id="port-input"
            value={port}
            onChange={(e) => {
              setPort(e.target.value);
              setPortDirty(true);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && portValid) create();
            }}
            disabled={creating}
            inputMode="numeric"
            placeholder="8080"
            aria-label="Exposed port"
            aria-invalid={!portValid}
            className={cn(!portValid && "border-err/60 focus:border-err")}
          />
          {portValid ? (
            <p className="text-[10px] leading-snug text-fg-muted">
              Port the container process must bind for the URL to route.
            </p>
          ) : (
            <p className="text-[10px] leading-snug text-err">
              Must be a TCP port (1–65535).
            </p>
          )}
        </div>
        <DisclosureSection
          label="Env vars"
          badge={envParsed.count > 0 ? envParsed.count : undefined}
          open={envOpen}
          onToggle={() => setEnvOpen((o) => !o)}
        >
          <Textarea
            value={envText}
            onChange={(e) => setEnvText(e.target.value)}
            placeholder="KEY=value"
            disabled={creating}
            // Match the Input field's size + a slightly tighter
            // min-height so the empty state doesn't dominate. The
            // format hint moves out of the placeholder to the helper
            // line below — placeholder was acting as multi-line in-
            // textarea documentation and reading way too loud.
            className="min-h-[64px] text-[12px]"
            aria-label="Environment variables (KEY=value per line)"
          />
          <p className="text-[10px] leading-snug text-fg-muted">
            One <span className="text-fg">KEY=value</span> per line.{" "}
            <span className="text-fg">#</span> for comments. Paste from
            a .env.
          </p>
          {envParsed.invalidLines.length > 0 && (
            <ul className="space-y-0.5 text-[10px] leading-snug text-warn">
              {envParsed.invalidLines.map((l) => (
                <li key={l.lineNumber}>
                  line {l.lineNumber}: {l.reason}
                </li>
              ))}
            </ul>
          )}
        </DisclosureSection>
        <div className="space-y-1.5 pt-1">
          <div className="flex gap-2">
            <Button
              onClick={create}
              disabled={creating || !image.trim() || !portValid}
              className="flex-1"
            >
              {creating ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <Plus className="size-3.5" />
              )}
              {creating
                ? "Creating…"
                : willAutorun
                  ? "Create & run"
                  : "Create"}
            </Button>
            <Button
              variant="secondary"
              onClick={onMutated}
              disabled={refreshing}
              title="Refresh sandbox list"
            >
              <RefreshCw
                className={cn("size-3.5", refreshing && "animate-spin")}
              />
            </Button>
          </div>
          <div className="text-center text-[10.5px] text-fg-muted">
            or{" "}
            <button
              type="button"
              onClick={createBlank}
              disabled={creating}
              title="Bare alpine container. No URL until you start a process on :8080."
              className="underline-offset-2 transition-colors hover:text-accent hover:underline disabled:opacity-50"
            >
              create a blank sandbox
            </button>
          </div>
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

function DisclosureSection({
  label,
  badge,
  open,
  onToggle,
  children,
}: {
  label: string;
  /** Right-side hint shown next to the header when collapsed and
   *  populated (e.g. env-var count, non-default tier name). Keeps
   *  state visible without forcing the user to expand. */
  badge?: string | number;
  open: boolean;
  onToggle: () => void;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      {/* Sentence-case + hover background to read as a button/toggle,
          NOT a field label. Required-field labels above (TEMPLATE,
          IMAGE, PORT) keep their uppercase/tracked treatment, so the
          two roles are visually distinct: labels describe the field
          below them, disclosure headers commit an open/close action. */}
      <button
        type="button"
        onClick={onToggle}
        className="-mx-1.5 flex w-[calc(100%+0.75rem)] items-center justify-between gap-2 rounded-md px-1.5 py-1 text-left text-[12px] font-medium text-fg-muted transition-colors hover:bg-surface-2 hover:text-fg"
        aria-expanded={open}
      >
        <span className="flex items-center gap-1.5">
          <ChevronDown
            className={cn(
              "size-3.5 transition-transform",
              open && "rotate-180",
            )}
          />
          {label}
        </span>
        {!open && badge !== undefined && badge !== "" && (
          <span className="rounded bg-surface-2 px-1.5 py-0.5 text-[10.5px] text-fg">
            {badge}
          </span>
        )}
      </button>
      {open && <div className="space-y-1.5">{children}</div>}
    </div>
  );
}
