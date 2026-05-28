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

import { useCallback, useState } from "react";

import type { ApiConfig } from "@/lib/api";
import { ApiError, api } from "@/lib/api";
import { FileTree } from "@/components/file-tree";
import { Editor, type OpenTab, type SaveStatus } from "@/components/editor";

interface Props {
  config: ApiConfig;
  sandboxId: string;
}

export function LiveEditPanel({ config, sandboxId }: Props) {
  const [tabs, setTabs] = useState<OpenTab[]>([]);
  const [activePath, setActivePath] = useState<string | null>(null);
  const [status, setStatus] = useState<SaveStatus>({ kind: "idle" });

  const openFile = useCallback(
    async (absPath: string) => {
      // If the file is already open, just focus its tab.
      if (tabs.some((t) => t.path === absPath)) {
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
        // doesn't blow up the editor. The user can still see the
        // bytes as garbled chars and re-save them; binary editing
        // is out of scope for v1.0.3.
        const content = new TextDecoder("utf-8", { fatal: false }).decode(
          bytes,
        );
        setTabs((prev) => [
          ...prev,
          { path: absPath, content, revision, dirty: false },
        ]);
        setActivePath(absPath);
      } catch (e) {
        const message =
          e instanceof ApiError
            ? `${e.errorCode ?? e.status}: ${e.message}`
            : String(e);
        setStatus({ kind: "error", path: absPath, message });
      }
    },
    [config, sandboxId, tabs],
  );

  const closeTab = useCallback(
    (path: string) => {
      setTabs((prev) => {
        const next = prev.filter((t) => t.path !== path);
        // If we just closed the active tab, focus the previous
        // tab in the strip (or null if none remain).
        if (path === activePath) {
          const idx = prev.findIndex((t) => t.path === path);
          const fallback =
            next[Math.max(0, idx - 1)] ?? next[0] ?? null;
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
          ? { ...t, content, dirty: t.content !== content || t.dirty }
          : t,
      ),
    );
  }, []);

  const onSave = useCallback(
    async (path: string) => {
      const tab = tabs.find((t) => t.path === path);
      if (!tab) return;
      setStatus({ kind: "saving", path });
      try {
        const res = await api.writeFile(config, sandboxId, path, tab.content, {
          // v1.0.3: pass the cached revision so concurrent
          // external edits surface as 409 REVISION_MISMATCH
          // instead of silently last-write-wins. An empty
          // revision (runtime doesn't support stat_revision)
          // means "no precondition" per the wire contract.
          expectedRevision: tab.revision ?? "",
        });
        setTabs((prev) =>
          prev.map((t) =>
            t.path === path
              ? {
                  ...t,
                  // Refresh the cached revision from the
                  // FileMeta sidecar. When the agent runtime
                  // doesn't emit one (e.g. a v1.0.2 fleet
                  // straggler), keep the previously-cached value
                  // so the next save still uses it.
                  revision: res.revision ?? t.revision,
                  dirty: false,
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
    [config, sandboxId, tabs],
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
