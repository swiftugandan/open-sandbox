"use client";

/** v1.0.3 live-edit file tree (PLAN_LIVE_EDIT_TASKS D2-D4).
 *
 *  Lazy one-level expansion — each directory's children are fetched
 *  on demand via `api.listDir` and cached. No `tree?depth=N` request
 *  shape: the moment a user expands `node_modules` the depth=N call
 *  would DoS the agent's readdir budget. Per the plan, expand-on-
 *  click is the only way new entries reach the UI.
 *
 *  Default-hidden directories (node_modules, .git, target, …) are
 *  filtered client-side. `Cmd-Shift-H` toggles the filter.
 *
 *  The component is intentionally narrow: it owns ONLY the tree
 *  navigation state. The editor / preview consume `onSelect` to
 *  open the chosen file. Conflict UX (D11) and watch-driven refresh
 *  (D3) layer on top.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronRight,
  ChevronDown,
  File,
  FilePlus,
  Folder,
  RefreshCw,
  Trash2,
} from "lucide-react";

import type { ApiConfig } from "@/lib/api";
import { ApiError, api, type ListDirEntry } from "@/lib/api";
import { isHiddenByDefault } from "@/lib/tree-defaults";
import { Button } from "@/components/ui/button";
import { useConfirm } from "@/components/ui/confirm-dialog";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/cn";

/** Default root path the tree opens at. Mirrors the agent's
 *  `DEFAULT_WRITE_CWD` (`/home`) but the file tree typically wants
 *  `/workspace` — pass through as a prop so callers can override. */
export const DEFAULT_TREE_ROOT = "/workspace";

interface Props {
  config: ApiConfig;
  sandboxId: string;
  /** Absolute path the tree opens at. Defaults to /workspace. */
  rootPath?: string;
  /** Called when a file leaf is clicked. The selected path is the
   *  absolute path inside the sandbox. */
  onSelect?: (absPath: string) => void;
  /** Path of the currently-selected leaf, for the highlighted-row UI. */
  selectedPath?: string;
}

interface DirState {
  /** True once we've fetched at least once. */
  loaded: boolean;
  /** Children sorted: dirs first (alphabetical), then files (alphabetical). */
  entries: ListDirEntry[];
  /** Server-side cap. UI shows a "+N more not shown" banner. */
  truncated: boolean;
  totalEntries: number;
  /** Per-directory error state (e.g. permission denied, sandbox gone). */
  error?: string;
  /** True while the listDir call is in flight. */
  loading: boolean;
}

interface ExpandState {
  /** Per-path tree state. Key is absolute path, e.g. "/workspace/src". */
  dirs: Map<string, DirState>;
  /** Paths the user has opened. */
  expanded: Set<string>;
}

function emptyExpandState(): ExpandState {
  return { dirs: new Map(), expanded: new Set() };
}

function sortEntries(es: ListDirEntry[]): ListDirEntry[] {
  // Dirs (and symlinks pointing at dirs — we can't tell without
  // following, so keep them with the leaves) first, then files,
  // alphabetical within each group. Matches the common Finder /
  // file-manager affordance.
  return [...es].sort((a, b) => {
    const aDir = a.type === "dir";
    const bDir = b.type === "dir";
    if (aDir && !bDir) return -1;
    if (!aDir && bDir) return 1;
    return a.name.localeCompare(b.name);
  });
}

export function FileTree({
  config,
  sandboxId,
  rootPath: rawRootPath = DEFAULT_TREE_ROOT,
  onSelect,
  selectedPath,
}: Props) {
  // Defensive: callers occasionally pass paths with trailing slashes
  // (copy-pasted from a UI breadcrumb, etc). Normalize once at the
  // boundary so the rest of the component doesn't have to special-
  // case `"/workspace/" === "/workspace"` and the `abs` composition
  // can't produce `//src` double-slashes.
  const rootPath = useMemo(
    () =>
      rawRootPath.length > 1 && rawRootPath.endsWith("/")
        ? rawRootPath.replace(/\/+$/, "")
        : rawRootPath,
    [rawRootPath],
  );
  const [state, setState] = useState<ExpandState>(emptyExpandState);
  const [showHidden, setShowHidden] = useState(false);
  // The root path is always conceptually "expanded" so the user sees
  // its children on first paint. We don't add it to `expanded` (which
  // tracks user-toggled state); we just always render its children.
  const [refreshNonce, setRefreshNonce] = useState(0);
  // Generation token incremented on every (sandboxId, rootPath,
  // refreshNonce) change. Any in-flight listDir captures the
  // generation in scope at the time it was dispatched and only writes
  // its result back when the generation is still current. Closes the
  // sandbox-switch race the v5 code-review pass flagged.
  const generationRef = useRef(0);

  /** Stable mutator that overlays a partial DirState. */
  const updateDir = useCallback(
    (path: string, patch: Partial<DirState> | ((prev: DirState | undefined) => DirState)) => {
      setState((s) => {
        const dirs = new Map(s.dirs);
        const prev = dirs.get(path);
        const next =
          typeof patch === "function"
            ? patch(prev)
            : {
                loaded: prev?.loaded ?? false,
                entries: prev?.entries ?? [],
                truncated: prev?.truncated ?? false,
                totalEntries: prev?.totalEntries ?? 0,
                loading: prev?.loading ?? false,
                error: prev?.error,
                ...patch,
              };
        dirs.set(path, next);
        return { ...s, dirs };
      });
    },
    [],
  );

  /** Issue a listDir for `path` and merge the result.
   *
   *  Captures the current `generationRef` value at dispatch time.
   *  When the response arrives the result is only committed to
   *  state if the generation is still current — otherwise it
   *  belongs to a sandbox we've already switched away from (or a
   *  root the user has navigated past) and writing it would
   *  paint stale tree contents over the live view. */
  const fetchDir = useCallback(
    async (path: string) => {
      const myGeneration = generationRef.current;
      updateDir(path, { loading: true, error: undefined });
      try {
        const res = await api.listDir(config, sandboxId, path);
        if (generationRef.current !== myGeneration) return;
        updateDir(path, {
          loaded: true,
          loading: false,
          entries: sortEntries(res.entries),
          truncated: res.truncated,
          totalEntries: res.total_entries,
          error: undefined,
        });
      } catch (e) {
        if (generationRef.current !== myGeneration) return;
        const detail =
          e instanceof ApiError ? `${e.errorCode ?? e.status}: ${e.message}` : String(e);
        updateDir(path, { loading: false, error: detail });
      }
    },
    [config, sandboxId, updateDir],
  );

  // Root always loads on mount + when sandbox/root changes. Bumping
  // the generation here causes any in-flight fetchDir from the
  // previous identity to drop its result on arrival.
  useEffect(() => {
    generationRef.current += 1;
    setState(emptyExpandState());
    fetchDir(rootPath);
  }, [rootPath, sandboxId, refreshNonce, fetchDir]);

  // Cmd-Shift-H (Ctrl-Shift-H on Linux/Windows): toggle hidden-dir
  // visibility. Bound at the document level so the focus target
  // doesn't matter; the editor swallows other shortcuts but this
  // global toggle survives.
  //
  // Skip when the user is typing in an input / textarea /
  // contenteditable surface — they almost certainly meant the chord
  // for whatever editor they're in (CodeMirror, a search box, etc).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (
        !(e.metaKey || e.ctrlKey) ||
        !e.shiftKey ||
        e.key.toLowerCase() !== "h"
      ) {
        return;
      }
      const target = e.target as HTMLElement | null;
      if (target) {
        const tag = target.tagName;
        if (
          tag === "INPUT" ||
          tag === "TEXTAREA" ||
          target.isContentEditable
        ) {
          return;
        }
      }
      e.preventDefault();
      setShowHidden((v) => !v);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const toggleExpand = useCallback(
    (path: string) => {
      setState((s) => {
        const expanded = new Set(s.expanded);
        if (expanded.has(path)) {
          expanded.delete(path);
        } else {
          expanded.add(path);
        }
        return { ...s, expanded };
      });
      // Lazy-load on expansion. The Set update above is async via
      // setState; we don't need to read the post-update value because
      // fetchDir is idempotent — re-fetching on a re-collapse is a
      // ~1KB GET which beats wiring a stale-closure check.
      const dir = state.dirs.get(path);
      if (!dir || !dir.loaded) {
        void fetchDir(path);
      }
    },
    [state.dirs, fetchDir],
  );

  const confirm = useConfirm();

  // v1.0.3 D2.5: inline "new file" affordance. `newFileRow` is
  // the open input state (null when closed). The flow runs
  // entirely inside the FileTree component — no native
  // window.prompt / window.alert; matches the rest of the dev
  // console's React-confirm-dialog pattern.
  const [newFileRow, setNewFileRow] = useState<NewFileRowState | null>(null);
  const submitNewFile = useCallback(
    async (rawInput: string) => {
      const trimmed = rawInput.trim();
      if (!trimmed) {
        setNewFileRow(null);
        return;
      }
      // Absolute path → honor verbatim. Otherwise root at the
      // tree's rootPath. The agent's write_file does `mkdir -p`
      // on the parent, so `src/api/users.py` creates the
      // intermediate dirs.
      const absPath = trimmed.startsWith("/")
        ? trimmed
        : `${rootPath}/${trimmed}`;
      setNewFileRow({ busy: true, value: trimmed, error: null });
      try {
        await api.writeFile(config, sandboxId, absPath, "");
        setNewFileRow(null);
        setRefreshNonce((n) => n + 1);
        onSelect?.(absPath);
      } catch (e) {
        const message =
          e instanceof ApiError
            ? `${e.errorCode ?? e.status}: ${e.message}`
            : String(e);
        setNewFileRow({ busy: false, value: trimmed, error: message });
      }
    },
    [config, sandboxId, rootPath, onSelect],
  );

  /** Confirm + delete a file or directory. Re-uses the dev
   *  console's React `ConfirmDialog` rather than a native
   *  browser confirm. Refreshes the tree on success. */
  const onDelete = useCallback(
    async (absPath: string, isDir: boolean) => {
      const basename = absPath.slice(absPath.lastIndexOf("/") + 1);
      const ok = await confirm({
        title: isDir ? "Delete directory?" : "Delete file?",
        description: (
          <>
            <span className="font-mono">{basename}</span>
            {isDir
              ? " and all its contents will be deleted from the sandbox."
              : " will be deleted from the sandbox."}
          </>
        ),
        confirmLabel: "Delete",
        variant: "danger",
      });
      if (!ok) return;
      try {
        // The caller may have passed isDir=false on a row the
        // server in fact reports as a directory (e.g. a symlink
        // to a dir, or stale tree state). We pass `recursive:
        // isDir` so the dir-path is auto-recursive; for non-
        // directories the agent treats `recursive=true` the
        // same as `rm -rf` on a leaf (no-op flag — `rm -rf foo`
        // works on files too).
        await api.deleteFile(config, sandboxId, absPath, { recursive: isDir });
        setRefreshNonce((n) => n + 1);
      } catch (e) {
        // v1.0.3: 409 DIRECTORY_NOT_EMPTY is the typed-conflict
        // case the agent emits when the user clicked "delete"
        // on something the tree thought was a file but the
        // server resolved as a populated dir (e.g. a stale
        // listing). Re-prompt with the recursive option.
        if (
          e instanceof ApiError &&
          e.errorCode === "DIRECTORY_NOT_EMPTY"
        ) {
          const recur = await confirm({
            title: "Directory not empty",
            description: (
              <>
                <span className="font-mono">{basename}</span>
                {" is a directory and has contents. Delete it and everything inside?"}
              </>
            ),
            confirmLabel: "Delete recursively",
            variant: "danger",
          });
          if (recur) {
            try {
              await api.deleteFile(config, sandboxId, absPath, {
                recursive: true,
              });
              setRefreshNonce((n) => n + 1);
            } catch (e2) {
              const message =
                e2 instanceof ApiError
                  ? `${e2.errorCode ?? e2.status}: ${e2.message}`
                  : String(e2);
              await confirm({
                title: "Delete failed",
                description: message,
                confirmLabel: "OK",
              });
            }
          }
          return;
        }
        // Re-confirm dialog with the error message — keeps the
        // user inside the modal flow instead of a separate
        // toast they might miss.
        const message =
          e instanceof ApiError
            ? `${e.errorCode ?? e.status}: ${e.message}`
            : String(e);
        await confirm({
          title: "Delete failed",
          description: message,
          confirmLabel: "OK",
        });
      }
    },
    [confirm, config, sandboxId],
  );

  return (
    <div className="flex flex-col h-full text-[12px] font-mono">
      <TreeHeader
        rootPath={rootPath}
        showHidden={showHidden}
        onToggleHidden={() => setShowHidden((v) => !v)}
        onRefresh={() => setRefreshNonce((n) => n + 1)}
        onNewFile={() =>
          setNewFileRow((prev) =>
            prev ? prev : { busy: false, value: "", error: null },
          )
        }
      />
      {newFileRow && (
        <NewFileRow
          state={newFileRow}
          onChange={(value) =>
            setNewFileRow((prev) =>
              prev ? { ...prev, value, error: null } : prev,
            )
          }
          onSubmit={submitNewFile}
          onCancel={() => setNewFileRow(null)}
        />
      )}
      <div className="flex-1 overflow-auto py-1">
        <DirChildren
          path={rootPath}
          state={state}
          showHidden={showHidden}
          onToggleExpand={toggleExpand}
          onSelect={onSelect}
          onDelete={onDelete}
          selectedPath={selectedPath}
          depth={0}
        />
      </div>
    </div>
  );
}

interface NewFileRowState {
  /** Whether the writeFile request is in flight. */
  busy: boolean;
  /** Current input value. */
  value: string;
  /** Last error message, surfaced inline below the input. */
  error: string | null;
}

function NewFileRow({
  state,
  onChange,
  onSubmit,
  onCancel,
}: {
  state: NewFileRowState;
  onChange: (value: string) => void;
  onSubmit: (value: string) => void;
  onCancel: () => void;
}) {
  const inputRef = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    inputRef.current?.focus();
  }, []);
  return (
    <div className="flex flex-col gap-1 px-2 py-1.5 border-b border-border bg-surface-1">
      <Input
        ref={inputRef}
        value={state.value}
        placeholder="filename (e.g. src/app.py)"
        disabled={state.busy}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            onSubmit(state.value);
          } else if (e.key === "Escape") {
            e.preventDefault();
            onCancel();
          }
        }}
        className="h-7 text-[11.5px]"
      />
      {state.error && (
        <span className="text-[11px] text-err" role="alert">
          {state.error}
        </span>
      )}
    </div>
  );
}

function TreeHeader({
  rootPath,
  showHidden,
  onToggleHidden,
  onRefresh,
  onNewFile,
}: {
  rootPath: string;
  showHidden: boolean;
  onToggleHidden: () => void;
  onRefresh: () => void;
  onNewFile: () => void;
}) {
  return (
    <div className="flex items-center gap-2 px-2 py-1.5 border-b border-border text-fg-muted">
      <span className="truncate" title={rootPath}>
        {rootPath}
      </span>
      <span className="flex-1" />
      <Button
        size="icon"
        variant="ghost"
        title="New file (relative to /workspace; nested paths create parents)"
        onClick={onNewFile}
      >
        <FilePlus size={12} />
      </Button>
      <Button
        size="icon"
        variant="ghost"
        title={
          showHidden
            ? "Hide default-hidden directories (⇧⌘H)"
            : "Show default-hidden directories (⇧⌘H)"
        }
        onClick={onToggleHidden}
        aria-pressed={showHidden}
      >
        {/* Use a simple text indicator rather than a separate icon */}
        <span className="text-[10px] font-semibold">{showHidden ? "•" : "○"}</span>
      </Button>
      <Button
        size="icon"
        variant="ghost"
        title="Refresh"
        onClick={onRefresh}
      >
        <RefreshCw size={12} />
      </Button>
    </div>
  );
}

interface DirChildrenProps {
  path: string;
  state: ExpandState;
  showHidden: boolean;
  onToggleExpand: (path: string) => void;
  onSelect?: (absPath: string) => void;
  onDelete: (absPath: string, isDir: boolean) => void | Promise<void>;
  selectedPath?: string;
  depth: number;
}

function DirChildren(props: DirChildrenProps) {
  const { path, state, showHidden, depth } = props;
  const dir = state.dirs.get(path);
  if (!dir) {
    // Root tree not loaded yet — happens for one frame after mount.
    return <div className="px-2 text-fg-muted">…</div>;
  }
  if (dir.error) {
    return (
      <div className="px-2 py-1 text-err" title={dir.error}>
        ✗ {dir.error}
      </div>
    );
  }
  if (dir.loading && !dir.loaded) {
    return <div className="px-2 text-fg-muted">loading…</div>;
  }
  // Hidden-dir filter also matches symlinks — pnpm and yarn
  // workspaces routinely symlink `node_modules`, and a bare
  // `e.type === 'dir'` check would let those slip through and
  // defeat the whole default-hidden affordance.
  const visible = dir.entries.filter((e) =>
    showHidden
      ? true
      : !(
          (e.type === "dir" || e.type === "symlink") &&
          isHiddenByDefault(e.name)
        ),
  );
  if (visible.length === 0 && dir.totalEntries === 0) {
    return <div className="px-2 text-fg-muted italic">(empty)</div>;
  }
  return (
    <ul className="select-none">
      {visible.map((entry) => (
        <TreeRow
          key={entry.name}
          parentPath={path}
          entry={entry}
          state={state}
          showHidden={showHidden}
          onToggleExpand={props.onToggleExpand}
          onSelect={props.onSelect}
          onDelete={props.onDelete}
          selectedPath={props.selectedPath}
          depth={depth}
        />
      ))}
      {dir.truncated && (
        <li
          className="px-2 py-1 text-fg-muted italic"
          title={`Hit the ${dir.totalEntries}-entry server cap. Drill into a subdirectory to see more.`}
        >
          … {dir.totalEntries - dir.entries.length}+ more not shown
        </li>
      )}
    </ul>
  );
}

function TreeRow({
  parentPath,
  entry,
  state,
  showHidden,
  onToggleExpand,
  onSelect,
  onDelete,
  selectedPath,
  depth,
}: {
  parentPath: string;
  entry: ListDirEntry;
  state: ExpandState;
  showHidden: boolean;
  onToggleExpand: (path: string) => void;
  onSelect?: (absPath: string) => void;
  onDelete: (absPath: string, isDir: boolean) => void | Promise<void>;
  selectedPath?: string;
  depth: number;
}) {
  const abs = useMemo(
    () =>
      parentPath === "/" ? `/${entry.name}` : `${parentPath}/${entry.name}`,
    [parentPath, entry.name],
  );
  const isDir = entry.type === "dir";
  const isExpanded = isDir && state.expanded.has(abs);
  const isSelected = selectedPath === abs;
  const indent = depth * 12;

  const onClick = useCallback(() => {
    if (isDir) {
      onToggleExpand(abs);
    } else {
      onSelect?.(abs);
    }
  }, [isDir, abs, onToggleExpand, onSelect]);

  return (
    <li>
      <div
        className={cn(
          "group flex items-center gap-1 pr-1",
          "hover:bg-surface-2",
          isSelected && "bg-surface-2 text-fg",
          !isSelected && "text-fg-muted",
        )}
      >
        <button
          type="button"
          onClick={onClick}
          title={abs}
          className="min-w-0 flex-1 flex items-center gap-1 py-0.5 text-left"
          style={{ paddingLeft: 8 + indent }}
        >
          {isDir ? (
            isExpanded ? (
              <ChevronDown size={12} className="shrink-0" />
            ) : (
              <ChevronRight size={12} className="shrink-0" />
            )
          ) : (
            <span className="w-3 shrink-0" />
          )}
          {isDir ? (
            <Folder size={12} className="shrink-0" />
          ) : (
            <File size={12} className="shrink-0" />
          )}
          <span className="truncate">{entry.name}</span>
          {entry.type === "symlink" && entry.target && (
            <span className="text-fg-muted/60 ml-1">→ {entry.target}</span>
          )}
        </button>
        <button
          type="button"
          // The trash button only appears on row hover (or
          // keyboard focus) so the tree stays visually quiet.
          // stopPropagation so the parent row's onClick (which
          // would open the file in the editor) doesn't fire.
          onClick={(e) => {
            e.stopPropagation();
            void onDelete(abs, isDir);
          }}
          title={isDir ? `Delete directory ${entry.name}` : `Delete ${entry.name}`}
          aria-label={isDir ? `Delete directory ${entry.name}` : `Delete file ${entry.name}`}
          className={cn(
            "shrink-0 p-1 opacity-0 transition-opacity",
            "group-hover:opacity-60 hover:!opacity-100 focus-visible:opacity-100",
            "text-fg-muted hover:text-err focus-visible:text-err",
            "focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-err/40 rounded",
          )}
        >
          <Trash2 size={11} />
        </button>
      </div>
      {isExpanded && (
        <DirChildren
          path={abs}
          state={state}
          showHidden={showHidden}
          onToggleExpand={onToggleExpand}
          onSelect={onSelect}
          onDelete={onDelete}
          selectedPath={selectedPath}
          depth={depth + 1}
        />
      )}
    </li>
  );
}
