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

import { useCallback, useRef, useState } from "react";

import type { ApiConfig, Sandbox } from "@/lib/api";
import { ApiError, api, publicUrl } from "@/lib/api";
import { FileTree } from "@/components/file-tree";
import { Editor, type OpenTab, type SaveStatus } from "@/components/editor";
import { PreviewPane } from "@/components/preview-pane";

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
  const scheduleReload = useCallback(() => {
    if (reloadDebounceRef.current !== null) {
      window.clearTimeout(reloadDebounceRef.current);
    }
    reloadDebounceRef.current = window.setTimeout(() => {
      reloadDebounceRef.current = null;
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
        const content = new TextDecoder("utf-8", { fatal: false }).decode(
          bytes,
        );
        setTabs((prev) => {
          // Another openFile won the race — bail rather than
          // appending a duplicate.
          if (prev.some((t) => t.path === absPath)) return prev;
          return [
            ...prev,
            {
              path: absPath,
              content,
              savedContent: content,
              revision,
              dirty: false,
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

  const onChange = useCallback((path: string, content: string) => {
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
  }, []);

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
    [config, sandboxId],
  );

  return (
    <div className="flex h-full min-h-0">
      <div className="w-[220px] shrink-0 border-r border-border bg-surface-1">
        <FileTree
          config={config}
          sandboxId={sandboxId}
          onSelect={openFile}
          selectedPath={activePath ?? undefined}
        />
      </div>
      <div className="flex-1 min-w-0 border-r border-border">
        <Editor
          tabs={tabs}
          activePath={activePath}
          status={status}
          onSelectTab={setActivePath}
          onCloseTab={closeTab}
          onChange={onChange}
          onSave={onSave}
        />
      </div>
      <div className="flex-1 min-w-0">
        <PreviewPane
          publicUrl={previewUrl}
          status={sandbox.status}
          reloadKey={reloadKey}
          onManualReload={() => setReloadKey((k) => k + 1)}
          port={previewPort}
        />
      </div>
    </div>
  );
}
