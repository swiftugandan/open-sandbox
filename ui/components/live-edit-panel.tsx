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

import type { ApiConfig } from "@/lib/api";
import { ApiError, api } from "@/lib/api";
import { FileTree } from "@/components/file-tree";
import { Editor, type OpenTab, type SaveStatus } from "@/components/editor";

interface Props {
  config: ApiConfig;
  sandboxId: string;
}

/** Internal per-tab record. Extends the public OpenTab with a
 *  `savedContent` snapshot so `dirty` can flip back to false when
 *  the user manually reverts edits — comparing only against the
 *  PREVIOUS in-memory content (the v6 design) would leave the
 *  dirty-dot stuck on after an undo to the saved state. */
interface InternalTab extends OpenTab {
  savedContent: string;
}

export function LiveEditPanel({ config, sandboxId }: Props) {
  const [tabs, setTabs] = useState<InternalTab[]>([]);
  const [activePath, setActivePath] = useState<string | null>(null);
  const [status, setStatus] = useState<SaveStatus>({ kind: "idle" });

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
      <div className="flex-1 min-w-0">
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
    </div>
  );
}
