"use client";

/** v1.0.3 live-edit: CodeMirror 6 editor host with tabs.
 *
 *  PLAN_LIVE_EDIT_TASKS group D items D6-D8: tabbed editor;
 *  Cmd-S save with optimistic dirty-dot; 5s autosave-on-blur
 *  fallback.
 *
 *  Memoization gotcha (plan §Sharp edges #4): `@uiw/react-
 *  codemirror` re-creates the `EditorView` whenever `extensions`
 *  identity changes. The `extensions` array is `useMemo`'d on
 *  `[langId]` ONLY — every parent re-render that doesn't change
 *  the language reuses the same array reference, so cursor /
 *  scroll / undo state survives.
 */

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { Save, X, Circle, Loader2 } from "lucide-react";
import type { Extension } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";
import { languageIdFor, loadLanguage, type LanguageId } from "@/lib/lang";

/** One open file. Identified by its absolute path inside the
 *  sandbox; the path also keys the IndexedDB unsaved buffer the
 *  D9 work introduces. */
export interface OpenTab {
  /** Absolute path inside the sandbox. */
  path: string;
  /** Last-known text content (in-memory mirror). */
  content: string;
  /** Last revision token from a successful read or write. `null`
   *  when the runtime backend hasn't wired stat_revision yet. */
  revision: string | null;
  /** True when the in-memory `content` has diverged from what was
   *  last saved to the agent. */
  dirty: boolean;
}

/** Status banner string the parent renders. `idle` = no message.
 *  `saving` = spinner is shown on the toolbar. `saved` = brief
 *  flash that fades out. `error` = persists until next save. */
export type SaveStatus =
  | { kind: "idle" }
  | { kind: "saving"; path: string }
  | { kind: "saved"; path: string; at: number }
  | { kind: "error"; path: string; message: string };

interface Props {
  tabs: OpenTab[];
  activePath: string | null;
  onSelectTab: (path: string) => void;
  onCloseTab: (path: string) => void;
  /** Called when the editor's content for a path changes (any
   *  keystroke). Parent reconciles dirty state + IndexedDB buffer. */
  onChange: (path: string, content: string) => void;
  /** Called on Cmd/Ctrl-S OR on the autosave-on-blur fallback.
   *  Parent runs the write-chain (writeFile + waitPortListening
   *  + preview reload). */
  onSave: (path: string) => void | Promise<void>;
  status: SaveStatus;
  /** Autosave-on-blur grace period in ms. Defaults to 5000 per
   *  the plan; tests may pass smaller values. */
  blurAutosaveMs?: number;
}

export function Editor(props: Props) {
  const { tabs, activePath, onSelectTab } = props;
  const activeTab = useMemo(
    () => tabs.find((t) => t.path === activePath) ?? null,
    [tabs, activePath],
  );

  return (
    <div className="flex flex-col h-full bg-bg text-fg">
      <TabStrip
        tabs={tabs}
        activePath={activePath}
        status={props.status}
        onSelectTab={onSelectTab}
        onCloseTab={props.onCloseTab}
        onSave={props.onSave}
      />
      {activeTab ? (
        <EditorPane
          key={activeTab.path}
          tab={activeTab}
          onChange={props.onChange}
          onSave={props.onSave}
          blurAutosaveMs={props.blurAutosaveMs ?? 5_000}
        />
      ) : (
        <div className="flex-1 flex items-center justify-center text-fg-muted text-sm">
          No file open. Click a file in the tree to start editing.
        </div>
      )}
    </div>
  );
}

function TabStrip({
  tabs,
  activePath,
  status,
  onSelectTab,
  onCloseTab,
  onSave,
}: {
  tabs: OpenTab[];
  activePath: string | null;
  status: SaveStatus;
  onSelectTab: (path: string) => void;
  onCloseTab: (path: string) => void;
  onSave: (path: string) => void | Promise<void>;
}) {
  return (
    <div className="flex items-stretch border-b border-border bg-surface-1">
      <div className="flex-1 flex overflow-x-auto">
        {tabs.map((t) => {
          const isActive = t.path === activePath;
          return (
            <div
              key={t.path}
              className={cn(
                "group flex items-center gap-1 px-3 border-r border-border text-[12px]",
                isActive
                  ? "bg-bg text-fg"
                  : "bg-surface-1 text-fg-muted hover:text-fg",
              )}
            >
              <button
                type="button"
                onClick={() => onSelectTab(t.path)}
                className="flex items-center gap-1 py-1.5"
                title={t.path}
              >
                <DirtyIndicator dirty={t.dirty} saving={isSavingFor(status, t.path)} />
                <span className="truncate max-w-[200px] font-mono">
                  {basename(t.path)}
                </span>
              </button>
              <button
                type="button"
                onClick={() => onCloseTab(t.path)}
                className="opacity-0 group-hover:opacity-100 hover:text-fg p-0.5"
                title="Close"
                aria-label={`Close ${basename(t.path)}`}
              >
                <X size={12} />
              </button>
            </div>
          );
        })}
      </div>
      <StatusBar status={status} onSaveActive={() => activePath && onSave(activePath)} />
    </div>
  );
}

function DirtyIndicator({
  dirty,
  saving,
}: {
  dirty: boolean;
  saving: boolean;
}) {
  if (saving) {
    return (
      <Loader2 size={10} className="shrink-0 animate-spin" aria-label="saving" />
    );
  }
  if (dirty) {
    // Filled dot matches VS Code's "modified" indicator; clearer
    // than an asterisk in a dense tab strip (plan §Save model).
    return (
      <Circle
        size={10}
        fill="currentColor"
        className="shrink-0"
        aria-label="unsaved changes"
      />
    );
  }
  // Reserve the same horizontal slot so tabs don't reflow when
  // dirty flips.
  return <span className="w-2.5 shrink-0" aria-hidden />;
}

function StatusBar({
  status,
  onSaveActive,
}: {
  status: SaveStatus;
  onSaveActive: () => void;
}) {
  return (
    <div className="flex items-center gap-2 px-3 text-[11px] text-fg-muted">
      <StatusMessage status={status} />
      <Button
        size="icon"
        variant="ghost"
        title="Save (⌘S)"
        onClick={onSaveActive}
      >
        <Save size={12} />
      </Button>
    </div>
  );
}

function StatusMessage({ status }: { status: SaveStatus }) {
  const [tick, setTick] = useState(0);
  // Re-render every second while a "saved" message is showing so
  // the fade-out is purely declarative — the message disappears
  // once we're past the 1500ms window.
  useEffect(() => {
    if (status.kind !== "saved") return;
    const t = window.setInterval(() => setTick((n) => n + 1), 250);
    return () => window.clearInterval(t);
  }, [status.kind]);
  if (status.kind === "saving") {
    return <span>Saving {basename(status.path)}…</span>;
  }
  if (status.kind === "saved") {
    const age = Date.now() - status.at;
    if (age > 1500) return null;
    return (
      <span className="text-ok" aria-live="polite">
        Saved
      </span>
    );
  }
  if (status.kind === "error") {
    return (
      <span className="text-err" title={status.message}>
        Save failed
      </span>
    );
  }
  // Touch `tick` so the linter doesn't complain about it being
  // unused; the re-render side-effect IS the point.
  void tick;
  return null;
}

function isSavingFor(status: SaveStatus, path: string): boolean {
  return status.kind === "saving" && status.path === path;
}

function basename(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash >= 0 ? path.slice(slash + 1) : path;
}

/** The CodeMirror host for a single tab. Lazily loads the
 *  language extension on mount; until the load resolves the
 *  editor shows the file content with no syntax highlighting. */
function EditorPane({
  tab,
  onChange,
  onSave,
  blurAutosaveMs,
}: {
  tab: OpenTab;
  onChange: (path: string, content: string) => void;
  onSave: (path: string) => void | Promise<void>;
  blurAutosaveMs: number;
}) {
  const langId = useMemo(() => languageIdFor(tab.path), [tab.path]);
  const [langExt, setLangExt] = useState<Extension[]>([]);

  // Track the latest dirty state in a ref so the autosave-on-blur
  // closure doesn't have to be re-created on every keystroke.
  const dirtyRef = useRef(tab.dirty);
  useEffect(() => {
    dirtyRef.current = tab.dirty;
  }, [tab.dirty]);

  // Lazy-load the language extension. Restarts on path/langId
  // change (the parent's <EditorPane key={path}> remount also
  // handles this, but keep the effect-shape symmetric).
  useEffect(() => {
    let cancelled = false;
    if (!langId) {
      setLangExt([]);
      return;
    }
    loadLanguage(langId).then(
      (ext) => {
        if (!cancelled) setLangExt(ext);
      },
      () => {
        if (!cancelled) setLangExt([]);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [langId]);

  // Cmd/Ctrl-S binding. Memoized on the path so a parent re-
  // render doesn't trash the keymap (plan §Memoization gotcha).
  const saveKeymap = useMemo(
    () =>
      keymap.of([
        {
          key: "Mod-s",
          run: () => {
            void onSave(tab.path);
            return true;
          },
        },
      ]),
    [tab.path, onSave],
  );

  // Blur-autosave: when the editor loses focus AND the buffer is
  // dirty, fire onSave after `blurAutosaveMs`. Cancel on re-focus
  // or on a real save (handled by parent flipping dirty=false).
  const blurTimerRef = useRef<number | null>(null);
  const blurExtension = useMemo(
    () =>
      EditorView.domEventHandlers({
        blur: () => {
          if (blurTimerRef.current !== null) {
            window.clearTimeout(blurTimerRef.current);
          }
          blurTimerRef.current = window.setTimeout(() => {
            if (dirtyRef.current) {
              void onSave(tab.path);
            }
          }, blurAutosaveMs);
          return false;
        },
        focus: () => {
          if (blurTimerRef.current !== null) {
            window.clearTimeout(blurTimerRef.current);
            blurTimerRef.current = null;
          }
          return false;
        },
      }),
    [tab.path, onSave, blurAutosaveMs],
  );

  // The final extensions array. Reference-stable across re-
  // renders that don't change `langExt` / `saveKeymap` /
  // `blurExtension` — exactly the property `@uiw/react-codemirror`
  // needs to avoid trashing the EditorView on every keystroke.
  const extensions = useMemo(
    () => [...langExt, saveKeymap, blurExtension],
    [langExt, saveKeymap, blurExtension],
  );

  // Lazy-import `@uiw/react-codemirror` so the editor chunk isn't
  // in the initial bundle when no file is open.
  const CodeMirror = useLazyCodeMirror();
  if (!CodeMirror) {
    return (
      <div className="flex-1 flex items-center justify-center text-fg-muted text-sm">
        Loading editor…
      </div>
    );
  }
  return (
    <div className="flex-1 overflow-hidden">
      <CodeMirror
        value={tab.content}
        height="100%"
        theme="dark"
        extensions={extensions}
        onChange={(v: string) => onChange(tab.path, v)}
      />
    </div>
  );
}

// ─── Lazy import helper ────────────────────────────────────────

import type { ReactCodeMirrorRef, ReactCodeMirrorProps } from "@uiw/react-codemirror";

type CM6Component = React.ForwardRefExoticComponent<
  ReactCodeMirrorProps & React.RefAttributes<ReactCodeMirrorRef>
>;

let cachedCodeMirror: CM6Component | null = null;

function useLazyCodeMirror(): CM6Component | null {
  const [mod, setMod] = useState<CM6Component | null>(cachedCodeMirror);
  useEffect(() => {
    if (cachedCodeMirror) return;
    let cancelled = false;
    import("@uiw/react-codemirror").then((m) => {
      if (cancelled) return;
      cachedCodeMirror = m.default;
      setMod(m.default);
    });
    return () => {
      cancelled = true;
    };
  }, []);
  return mod;
}
