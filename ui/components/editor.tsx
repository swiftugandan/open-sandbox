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
import CodeMirror from "@uiw/react-codemirror";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";
import { languageIdFor, loadLanguage } from "@/lib/lang";

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
 *  flash that fades out. `error` = persists until next save.
 *  `conflict` = v1.0.3 optimistic-concurrency mismatch; the
 *  parent renders an actionable Reload / Overwrite banner above
 *  the editor pane. */
export type SaveStatus =
  | { kind: "idle" }
  | { kind: "saving"; path: string }
  | { kind: "saved"; path: string; at: number }
  | { kind: "error"; path: string; message: string }
  | {
      kind: "conflict";
      path: string;
      /** The current on-disk revision the agent reports — the
       *  client passes this back as the new `expected_revision`
       *  when the user clicks Reload. Empty string when the file
       *  no longer exists (someone deleted it underneath). */
      actualRevision: string;
    };

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
  /** Resolve a v1.0.3 REVISION_MISMATCH conflict by re-reading
   *  the file from the agent and replacing the in-memory buffer.
   *  Surfaced on the conflict banner. */
  onConflictReload: (path: string) => void | Promise<void>;
  /** Resolve a v1.0.3 REVISION_MISMATCH conflict by force-
   *  overwriting the on-disk content with the user's buffer.
   *  Surfaced on the conflict banner. */
  onConflictOverwrite: (path: string) => void | Promise<void>;
  status: SaveStatus;
  /** Autosave-on-blur grace period in ms. Defaults to 5000 per
   *  the plan; tests may pass smaller values. */
  blurAutosaveMs?: number;
}

export function Editor(props: Props) {
  const {
    tabs,
    activePath,
    onSelectTab,
    onSave,
    onConflictReload,
    onConflictOverwrite,
    status,
  } = props;
  const activeTab = useMemo(
    () => tabs.find((t) => t.path === activePath) ?? null,
    [tabs, activePath],
  );

  // Cmd/Ctrl-S: bound at the document level so the keystroke saves
  // the active tab regardless of whether focus is inside the
  // CodeMirror view, on a tab button, or on the file tree. CM6's
  // own keymap binding only fires when the editor itself has
  // focus — without this handler the browser intercepts the
  // chord and pops the "Save Page As…" dialog.
  //
  // Skip when the user is typing in a plain HTML INPUT / TEXTAREA
  // (the legacy FilesPanel, search boxes, etc); their containing
  // app may want the chord. CodeMirror's editor surface IS
  // contenteditable but we deliberately DO NOT skip there — the
  // intent is to save the file even from inside the editor.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.shiftKey || e.altKey) return;
      if (e.key.toLowerCase() !== "s") return;
      const target = e.target as HTMLElement | null;
      if (target) {
        const tag = target.tagName;
        if (tag === "INPUT" || tag === "TEXTAREA") return;
        // contenteditable surfaces OUTSIDE CodeMirror (a future
        // rich-text comment box, an extension overlay, etc) —
        // skip so we don't hijack a save the user meant for the
        // text they're typing. The CodeMirror surface IS
        // contenteditable; allow Cmd-S there by detecting the
        // `.cm-editor` ancestor.
        if (target.isContentEditable && !target.closest(".cm-editor")) {
          return;
        }
      }
      if (!activePath) return;
      e.preventDefault();
      void onSave(activePath);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [activePath, onSave]);

  return (
    <div className="flex flex-col h-full bg-bg text-fg">
      <TabStrip
        tabs={tabs}
        activePath={activePath}
        status={status}
        onSelectTab={onSelectTab}
        onCloseTab={props.onCloseTab}
        onSave={onSave}
      />
      {activeTab ? (
        <>
          {status.kind === "conflict" && status.path === activeTab.path && (
            <ConflictBanner
              path={activeTab.path}
              actualRevision={status.actualRevision}
              onReload={() => onConflictReload(activeTab.path)}
              onOverwrite={() => onConflictOverwrite(activeTab.path)}
            />
          )}
          <EditorPane
            key={activeTab.path}
            tab={activeTab}
            onChange={props.onChange}
            onSave={onSave}
            blurAutosaveMs={props.blurAutosaveMs ?? 5_000}
            status={status}
          />
        </>
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
  // Re-renders the StatusMessage once at the end of the
  // saved-flash window so the message fades out without an
  // interval running forever. Bumping `expired` to true is a
  // single setTimeout — no polling, no leak.
  const [expired, setExpired] = useState(false);
  useEffect(() => {
    setExpired(false);
    if (status.kind !== "saved") return;
    const remaining = Math.max(0, 1500 - (Date.now() - status.at));
    const t = window.setTimeout(() => setExpired(true), remaining);
    return () => window.clearTimeout(t);
  }, [status]);
  if (status.kind === "saving") {
    return <span>Saving {basename(status.path)}…</span>;
  }
  if (status.kind === "saved") {
    if (expired) return null;
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
  if (status.kind === "conflict") {
    // No inline label — the ConflictBanner above the editor is
    // the single source of conflict messaging; a duplicate
    // toolbar string just adds visual noise.
    return null;
  }
  return null;
}

function isSavingFor(status: SaveStatus, path: string): boolean {
  return status.kind === "saving" && status.path === path;
}

function basename(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash >= 0 ? path.slice(slash + 1) : path;
}

/** v1.0.3 D11 conflict banner — rendered above the editor pane
 *  when a writeFile returned 409 REVISION_MISMATCH. The user
 *  chooses between (a) Reload — discard local edits and re-read
 *  the file (the live revision becomes the new precondition for
 *  the next save), or (b) Overwrite — re-issue writeFile with
 *  force=true, last-write-wins. A Diff affordance is out of
 *  scope for v1.0.3 and tracked as a follow-up. */
function ConflictBanner({
  path,
  actualRevision,
  onReload,
  onOverwrite,
}: {
  path: string;
  actualRevision: string;
  onReload: () => void | Promise<void>;
  onOverwrite: () => void | Promise<void>;
}) {
  const detail =
    actualRevision === ""
      ? "The file was deleted on the agent. Reload will re-create it from your buffer; Overwrite will write your buffer back as a new file."
      : `${basename(path)} was changed on the agent since you opened it. Reload to fetch the latest content (your edits are discarded), or Overwrite to last-write-wins.`;
  return (
    <div
      role="alert"
      className="flex flex-wrap items-center gap-2 border-b border-warn/40 bg-warn/10 px-3 py-1.5 text-[12px] text-fg"
    >
      <span className="font-medium text-warn">Conflict</span>
      <span className="min-w-0 flex-1 truncate text-fg-muted">{detail}</span>
      <Button size="sm" variant="secondary" onClick={() => void onReload()}>
        Reload
      </Button>
      <Button size="sm" variant="danger" onClick={() => void onOverwrite()}>
        Overwrite
      </Button>
    </div>
  );
}

/** The CodeMirror host for a single tab. Lazily loads the
 *  language extension on mount; until the load resolves the
 *  editor shows the file content with no syntax highlighting. */
function EditorPane({
  tab,
  onChange,
  onSave,
  blurAutosaveMs,
  status,
}: {
  tab: OpenTab;
  onChange: (path: string, content: string) => void;
  onSave: (path: string) => void | Promise<void>;
  blurAutosaveMs: number;
  status: SaveStatus;
}) {
  const langId = useMemo(() => languageIdFor(tab.path), [tab.path]);
  const [langExt, setLangExt] = useState<Extension[]>([]);

  // Track the latest dirty state in a ref so the autosave-on-blur
  // closure doesn't have to be re-created on every keystroke.
  const dirtyRef = useRef(tab.dirty);
  useEffect(() => {
    dirtyRef.current = tab.dirty;
  }, [tab.dirty]);

  // Track whether a save is currently in flight for THIS tab.
  // Without this guard, a pending blur-autosave timer can fire a
  // second `onSave(path)` while the first call is still pending —
  // racing two concurrent writeFile against the agent with the
  // same revision token. See the v6 code-review pass for the
  // detailed scenario.
  const inFlightRef = useRef(false);
  useEffect(() => {
    inFlightRef.current = status.kind === "saving" && status.path === tab.path;
  }, [status, tab.path]);

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
      (err) => {
        // Surface the chunk-load failure to devtools so a CDN /
        // network issue is debuggable. The editor continues to
        // work as a plain-text editor; not worth a user-visible
        // banner.
        console.error(`failed to load CodeMirror language ${langId}:`, err);
        if (!cancelled) setLangExt([]);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [langId]);

  // Cmd-S is bound at the DOCUMENT level (see `Editor`'s
  // useEffect) so it fires regardless of focus target. A CM6
  // `Mod-s` keymap here would double-fire when focus is inside
  // the editor — both the document handler and the CM6 keymap
  // would invoke `onSave` for one keystroke, racing two writes
  // against the same revision token. So we deliberately do NOT
  // bind Mod-s here.
  //
  // The empty keymap is kept as a memo so the dependency-array
  // shape on `extensions` (below) is stable across iterations
  // of this design.
  const saveKeymap = useMemo(() => keymap.of([]), []);

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
            // Skip when a manual save is already in flight for
            // this tab — the user's Cmd-S is already running and
            // firing a second concurrent writeFile would race two
            // optimistic-concurrency preconditions against the
            // same revision token.
            if (dirtyRef.current && !inFlightRef.current) {
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
