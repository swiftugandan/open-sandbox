"use client";

/** v1.0.3 live-edit panel — host that wires the file tree, the
 *  CodeMirror editor, and (D13+) the preview iframe.
 *
 *  PLAN_LIVE_EDIT_TASKS group D integration step. The pre-D13
 *  shape is two columns: tree on the left, tabbed editor in the
 *  middle. Once D13 lands the preview iframe takes the
 *  rightmost slot.
 *
 *  State this component owns:
 *    * The set of OPEN TABS (what files the editor has loaded).
 *    * Per-tab content, dirty state, and revision token.
 *    * The current save-in-flight status (used to drive the
 *      Editor's toolbar / dirty-dot spinner state).
 *
 *  File reads + writes go through `api.readFile` / `api.writeFile`.
 *  The revision token is captured on each successful read AND
 *  refreshed from FileMeta on each successful write so subsequent
 *  optimistic-concurrency writes use the freshest token.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import type { ApiConfig, Sandbox } from "@/lib/api";
import { ApiError, api, publicUrl } from "@/lib/api";
import { FileTree } from "@/components/file-tree";
import { Editor, type OpenTab, type SaveStatus } from "@/components/editor";
import { PreviewPane } from "@/components/preview-pane";
import {
  getUnsavedBuffer,
  putUnsavedBuffer,
  removeUnsavedBuffer,
} from "@/lib/unsaved-buffer";

interface Props {
  config: ApiConfig;
  sandbox: Sandbox;
  /** In-container port the dev-server listens on. Defaults to
   *  8080 (matches the platform-wide DEFAULT_SANDBOX_EXPOSED_PORT).
   *  Surfaced so a future per-sandbox port-setting UI can plumb
   *  through without re-threading. */
  previewPort?: number;
}

/** Save chain: how long to wait for the in-container server to
 *  re-accept after a watchexec restart before giving up. 3s per
 *  PLAN_LIVE_EDIT.md; the gateway clamps to 5min. A failed wait
 *  doesn't block the iframe reload — we still bump the cache
 *  bust, the iframe just re-renders the same down-state. */
const WAIT_PORT_TIMEOUT_MS = 3_000;

/** Internal per-tab record. Extends the public OpenTab with a
 *  `savedContent` snapshot so `dirty` can flip back to false when
 *  the user manually reverts edits — comparing only against the
 *  PREVIOUS in-memory content (the v6 design) would leave the
 *  dirty-dot stuck on after an undo to the saved state. */
interface InternalTab extends OpenTab {
  savedContent: string;
}

export function LiveEditPanel({ config, sandbox, previewPort = 8080 }: Props) {
  const sandboxId = sandbox.sandbox_id;
  const [tabs, setTabs] = useState<InternalTab[]>([]);
  const [activePath, setActivePath] = useState<string | null>(null);
  const [status, setStatus] = useState<SaveStatus>({ kind: "idle" });
  // Monotonically increasing — PreviewPane re-creates the iframe
  // (with a `?__t=<reloadKey>` cache-bust) whenever this bumps.
  // Driven by both the save-chain and the manual Reload toolbar
  // button.
  const [reloadKey, setReloadKey] = useState(0);
  const previewUrl = publicUrl(config.base, sandbox.subdomain);

  // Debounce window for save-chain'd reloads — coalesces Cmd-S
  // mashing so we don't trigger four wait_port_listening + iframe
  // reload pairs in a row. Per PLAN_LIVE_EDIT.md §Preview reload.
  const reloadDebounceRef = useRef<number | null>(null);
  // Tracks whether this LiveEditPanel is still mounted, so the
  // fire-and-forget save-chain IIFE doesn't `setState` after a
  // sandbox switch (which remounts via the new `key` on the
  // parent), nor after a route navigation.
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (reloadDebounceRef.current !== null) {
        window.clearTimeout(reloadDebounceRef.current);
        reloadDebounceRef.current = null;
      }
    };
  }, []);
  const scheduleReload = useCallback(() => {
    if (!mountedRef.current) return;
    if (reloadDebounceRef.current !== null) {
      window.clearTimeout(reloadDebounceRef.current);
    }
    reloadDebounceRef.current = window.setTimeout(() => {
      reloadDebounceRef.current = null;
      if (!mountedRef.current) return;
      setReloadKey((k) => k + 1);
    }, 200);
  }, []);

  // `tabs` is read from inside `onSave` to recover the current
  // content + cached revision for a path. Putting `tabs` in
  // `onSave`'s useCallback deps would make the callback identity
  // change on every keystroke; that identity flows into the
  // Editor's saveKeymap / blurExtension `useMemo` deps, which
  // changes the `extensions` array reference, which forces
  // @uiw/react-codemirror to tear down + recreate the EditorView
  // — destroying cursor / scroll / undo state on every character.
  // The exact regression the v6 review caught. Read via a ref to
  // break the dep chain.
  const tabsRef = useRef<InternalTab[]>(tabs);
  tabsRef.current = tabs;

  const openFile = useCallback(
    async (absPath: string) => {
      // Race-safe duplicate-tab guard: gate INSIDE the functional
      // setTabs updater so two synchronous openFile calls (tree
      // double-click, two onSelect firings) can't both miss the
      // closure-stale `tabs` and append twice.
      let alreadyOpen = false;
      setTabs((prev) => {
        if (prev.some((t) => t.path === absPath)) {
          alreadyOpen = true;
          return prev;
        }
        return prev;
      });
      if (alreadyOpen) {
        setActivePath(absPath);
        return;
      }
      try {
        const { bytes, revision } = await api.readFile(
          config,
          sandboxId,
          absPath,
        );
        // The agent's read path is a binary stream; decode as
        // UTF-8 with `fatal: false` so a stray non-UTF8 byte
        // doesn't blow up the editor.
        const diskContent = new TextDecoder("utf-8", { fatal: false }).decode(
          bytes,
        );
        // v1.0.3 D9: check IndexedDB for an unsaved buffer
        // newer than what's on disk. If present AND different
        // from disk content, restore it (the user's lost work).
        // savedContent stays the disk snapshot so the dirty-dot
        // shows + the buffer can still be saved through the
        // normal path. On mismatch with the cached revision the
        // user's next save will surface as REVISION_MISMATCH —
        // the right outcome: it forces a conflict-banner
        // resolution rather than silently overwriting.
        const stash = await getUnsavedBuffer(sandboxId, absPath);
        const restoredFromStash =
          stash !== null && stash.content !== diskContent;
        const initialContent = restoredFromStash ? stash.content : diskContent;
        setTabs((prev) => {
          if (prev.some((t) => t.path === absPath)) return prev;
          return [
            ...prev,
            {
              path: absPath,
              content: initialContent,
              savedContent: diskContent,
              revision,
              dirty: restoredFromStash,
            },
          ];
        });
        setActivePath(absPath);
      } catch (e) {
        const message =
          e instanceof ApiError
            ? `${e.errorCode ?? e.status}: ${e.message}`
            : String(e);
        // Read failures route through a dedicated readError
        // status — Editor's StatusMessage would mislabel an
        // ApiError on read as "Save failed". For now, log to
        // console; D11's full conflict UX will surface this in
        // the tree row.
        console.error(`failed to open ${absPath}: ${message}`);
      }
    },
    [config, sandboxId],
  );

  const closeTab = useCallback(
    (path: string) => {
      setTabs((prev) => {
        const idx = prev.findIndex((t) => t.path === path);
        const next = prev.filter((t) => t.path !== path);
        if (path === activePath) {
          // Prefer the tab to the LEFT of the removed one when
          // possible; fall back to the new first tab; else null.
          const fallback = next[Math.max(0, idx - 1)] ?? next[0] ?? null;
          setActivePath(fallback?.path ?? null);
        }
        return next;
      });
    },
    [activePath],
  );

  const onChange = useCallback(
    (path: string, content: string) => {
      setTabs((prev) =>
        prev.map((t) =>
          t.path === path
            ? // Compare against the SAVED snapshot so a user who
              // manually reverts to the last-saved content sees the
              // dirty-dot clear. Comparing against the previous
              // in-memory content (the v6 shape) left dirty stuck
              // on after undo.
              { ...t, content, dirty: content !== t.savedContent }
            : t,
        ),
      );
      // v1.0.3 D9: persist or clear the IndexedDB unsaved buffer
      // so a browser reload restores the user's edits. Fire-and-
      // forget — IndexedDB writes are async + best-effort.
      const prevTab = tabsRef.current.find((t) => t.path === path);
      if (!prevTab) return;
      if (content === prevTab.savedContent) {
        void removeUnsavedBuffer(sandboxId, path);
      } else {
        void putUnsavedBuffer(sandboxId, path, content);
      }
    },
    [sandboxId],
  );

  const onSave = useCallback(
    async (path: string) => {
      // Read tabs via the ref so this callback's identity stays
      // stable across keystrokes — see the comment on `tabsRef`.
      const tab = tabsRef.current.find((t) => t.path === path);
      if (!tab) return;
      // Skip when the buffer is clean: a redundant save would
      // burn a write_file round-trip for no semantic change.
      if (!tab.dirty) return;
      setStatus({ kind: "saving", path });
      // Snapshot the content NOW; if the user types during the
      // network round-trip the new keystroke will flip dirty back
      // on, and we want the next save to use the latest content.
      const sentContent = tab.content;
      try {
        const res = await api.writeFile(config, sandboxId, path, sentContent, {
          expectedRevision: tab.revision ?? "",
        });
        setTabs((prev) =>
          prev.map((t) =>
            t.path === path
              ? {
                  ...t,
                  revision: res.revision ?? t.revision,
                  // Snapshot the content we sent as the new saved
                  // baseline. If the user typed during the round
                  // trip, t.content > sentContent and dirty stays
                  // true correctly.
                  savedContent: sentContent,
                  dirty: t.content !== sentContent,
                }
              : t,
          ),
        );
        setStatus({ kind: "saved", path, at: Date.now() });
        // v1.0.3 D9: clear the persisted unsaved buffer ONLY
        // when the post-save in-memory content equals what we
        // just wrote. If the user typed during the round trip,
        // the buffer is still dirty and the latest content
        // belongs in IndexedDB — re-persist instead.
        const postTab = tabsRef.current.find((t) => t.path === path);
        if (postTab && postTab.content === sentContent) {
          void removeUnsavedBuffer(sandboxId, path);
        } else if (postTab) {
          void putUnsavedBuffer(sandboxId, path, postTab.content);
        }
        // v1.0.3 save chain: writeFile succeeded → wait for the
        // in-container dev-server to come back up after watchexec
        // restarts the process → bump the preview iframe's
        // cache-bust so the next render shows the new build.
        // Don't await the wait — it's best-effort and the iframe
        // reload should happen even if the dev-server takes a
        // while (PreviewPane handles the down-state gracefully).
        void (async () => {
          try {
            await api.waitPortListening(
              config,
              sandboxId,
              previewPort,
              WAIT_PORT_TIMEOUT_MS,
            );
          } catch {
            // wait_port_listening failures (sandbox gone,
            // network blip) shouldn't block the user; the
            // preview will just be stale until the next save
            // or manual reload.
          }
          // mountedRef short-circuits setReloadKey inside
          // scheduleReload, so the IIFE completing after a
          // sandbox switch is a clean no-op.
          if (!mountedRef.current) return;
          scheduleReload();
        })();
      } catch (e) {
        // v1.0.3 D11: REVISION_MISMATCH gets the dedicated
        // conflict-banner UX. Other errors stay on the generic
        // 'Save failed' status indicator with the message
        // surfaced in the title tooltip.
        if (e instanceof ApiError && e.errorCode === "REVISION_MISMATCH") {
          setStatus({
            kind: "conflict",
            path,
            actualRevision: e.actualRevision ?? "",
          });
          return;
        }
        const message =
          e instanceof ApiError
            ? `${e.errorCode ?? e.status}: ${e.message}`
            : String(e);
        setStatus({ kind: "error", path, message });
      }
    },
    [config, sandboxId, previewPort, scheduleReload],
  );

  // v1.0.3 D12: periodic mtime check on the active file while
  // the tab is visible. Detects external edits (another ssh
  // session, a git pull) BEFORE the user hits save and is
  // confronted with the conflict banner. Runs only when the
  // browser tab is visible (no point polling a hidden tab) and
  // skips when the active tab is already dirty (the user will
  // see the conflict on save anyway; pre-warning would just be
  // noise).
  //
  // 30s cadence balances detection latency vs. agent load: each
  // poll is one stat_revision exec inside the container, so a
  // user with the editor open all day generates 2880 stats — a
  // negligible load.
  useEffect(() => {
    if (!activePath) return;
    const POLL_INTERVAL_MS = 30_000;
    let timer: number | null = null;
    const tick = async () => {
      if (document.visibilityState !== "visible") return;
      const tab = tabsRef.current.find((t) => t.path === activePath);
      if (!tab || tab.dirty || tab.revision === null) return;
      try {
        // Best-effort: read the file (the only revision-
        // exposing endpoint today). On 404 the file's gone; we
        // surface that as the same REVISION_MISMATCH-style
        // banner with an empty actualRevision. On match we
        // do nothing — the editor's view is still fresh.
        const { revision } = await api.readFile(
          config,
          sandboxId,
          activePath,
        );
        if (!mountedRef.current) return;
        // Spurious-conflict guard: if the tab opened against a
        // runtime that didn't support stat_revision (tab.revision
        // === null) and the agent has since started returning
        // real revisions, that capability change is NOT a file
        // mutation. Silently adopt the new revision as the
        // baseline instead of firing the conflict banner.
        if (tab.revision === null && revision !== null) {
          setTabs((prev) =>
            prev.map((t) =>
              t.path === activePath ? { ...t, revision } : t,
            ),
          );
          return;
        }
        if (revision !== null && revision !== tab.revision) {
          setStatus({
            kind: "conflict",
            path: activePath,
            actualRevision: revision,
          });
        }
      } catch (e) {
        if (e instanceof ApiError && e.errorCode === "FILE_NOT_FOUND") {
          setStatus({
            kind: "conflict",
            path: activePath,
            actualRevision: "",
          });
        }
        // Other errors (transient network, sandbox restart):
        // silent. The next save's error path will surface them.
      }
    };
    const onVisibility = () => {
      // Fire immediately on becoming visible so a long pause +
      // tab-return picks up changes without waiting a full
      // interval.
      if (document.visibilityState === "visible") void tick();
    };
    timer = window.setInterval(() => void tick(), POLL_INTERVAL_MS);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      if (timer !== null) window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [activePath, config, sandboxId]);

  /** D11: discard the local buffer and re-read the file from
   *  the agent. The disk content becomes the new in-memory
   *  buffer, the cached revision becomes the live one (so the
   *  next save's precondition is correct), and dirty clears. */
  const onConflictReload = useCallback(
    async (path: string) => {
      try {
        const { bytes, revision } = await api.readFile(config, sandboxId, path);
        const content = new TextDecoder("utf-8", { fatal: false }).decode(
          bytes,
        );
        setTabs((prev) =>
          prev.map((t) =>
            t.path === path
              ? { ...t, content, savedContent: content, revision, dirty: false }
              : t,
          ),
        );
        void removeUnsavedBuffer(sandboxId, path);
        setStatus({ kind: "idle" });
      } catch (e) {
        const message =
          e instanceof ApiError
            ? `${e.errorCode ?? e.status}: ${e.message}`
            : String(e);
        setStatus({ kind: "error", path, message });
      }
    },
    [config, sandboxId],
  );

  /** D11: force-overwrite. Re-issue writeFile with `force=true`
   *  (the documented escape hatch — see proto/proxy.proto's
   *  `WriteFileParams.force`). The agent skips the precondition
   *  check; whatever's on disk gets replaced with the buffer. */
  const onConflictOverwrite = useCallback(
    async (path: string) => {
      const tab = tabsRef.current.find((t) => t.path === path);
      if (!tab) return;
      setStatus({ kind: "saving", path });
      const sentContent = tab.content;
      try {
        const res = await api.writeFile(config, sandboxId, path, sentContent, {
          force: true,
        });
        setTabs((prev) =>
          prev.map((t) =>
            t.path === path
              ? {
                  ...t,
                  revision: res.revision ?? t.revision,
                  savedContent: sentContent,
                  dirty: t.content !== sentContent,
                }
              : t,
          ),
        );
        void removeUnsavedBuffer(sandboxId, path);
        setStatus({ kind: "saved", path, at: Date.now() });
        // Save chain — same shape as onSave's tail; force-
        // overwrites land the same bytes the watchexec restart
        // will pick up.
        void (async () => {
          try {
            await api.waitPortListening(
              config,
              sandboxId,
              previewPort,
              WAIT_PORT_TIMEOUT_MS,
            );
          } catch {
            /* swallowed per onSave's contract */
          }
          if (!mountedRef.current) return;
          scheduleReload();
        })();
      } catch (e) {
        const message =
          e instanceof ApiError
            ? `${e.errorCode ?? e.status}: ${e.message}`
            : String(e);
        setStatus({ kind: "error", path, message });
      }
    },
    [config, sandboxId, previewPort, scheduleReload],
  );

  return (
    <div className="flex h-full min-h-0">
      {/* v1.0.3 D16: below 768px we drop the tree + editor and
       *  render only the preview iframe with a "view on desktop
       *  to edit" hint. Editing on mobile is a deliberate non-goal
       *  per the plan — every serious browser editor either
       *  builds a native app or tells mobile users "this is a
       *  desktop product". Two months of CM6 mobile bug whack-a-
       *  mole is the wrong trade. */}
      <div className="hidden md:block w-[220px] shrink-0 border-r border-border bg-surface-1">
        <FileTree
          config={config}
          sandboxId={sandboxId}
          onSelect={openFile}
          selectedPath={activePath ?? undefined}
        />
      </div>
      <div className="hidden md:block flex-1 min-w-0 border-r border-border">
        <Editor
          tabs={tabs}
          activePath={activePath}
          status={status}
          onSelectTab={setActivePath}
          onCloseTab={closeTab}
          onChange={onChange}
          onSave={onSave}
          onConflictReload={onConflictReload}
          onConflictOverwrite={onConflictOverwrite}
        />
      </div>
      <div className="flex-1 min-w-0 flex flex-col">
        <div className="md:hidden border-b border-border bg-surface-1 px-3 py-2 text-[11.5px] text-fg-muted">
          Preview only on mobile — view on desktop to edit files.
        </div>
        <div className="flex-1 min-h-0">
          <PreviewPane
            publicUrl={previewUrl}
            status={sandbox.status}
            reloadKey={reloadKey}
            onManualReload={() => setReloadKey((k) => k + 1)}
            port={previewPort}
          />
        </div>
      </div>
    </div>
  );
}
