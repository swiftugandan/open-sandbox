/** v1.0.3 live-edit: IndexedDB-backed unsaved-content buffer.
 *
 *  PLAN_LIVE_EDIT_TASKS group D item D9. Persists the in-memory
 *  editor content for each {sandboxId, path} pair so a browser
 *  refresh / tab crash / Vercel hot-reload doesn't lose work.
 *
 *  Keying: `${sandboxId}::${path}`. Schema lives in `__store__`
 *  (one object store; key/value records). No background timer —
 *  callers `put` after every keystroke (cheap; IndexedDB writes
 *  are O(1) amortized) and `remove` on successful save.
 *
 *  Failure mode: IndexedDB unavailable (private window, browser
 *  config) → every operation no-ops and returns a sentinel; the
 *  editor still works, just without crash-safety. Promises never
 *  reject — failures are logged once and silently swallowed.
 */

const DB_NAME = "open-sandbox-live-edit";
const DB_VERSION = 1;
const STORE_NAME = "unsaved-buffers";

interface BufferRecord {
  /** `${sandboxId}::${path}` */
  key: string;
  sandboxId: string;
  path: string;
  content: string;
  /** Wall-clock ms; used for the optional restore-prompt UI. */
  savedAt: number;
}

let dbPromise: Promise<IDBDatabase | null> | null = null;
let warnedUnavailable = false;

function openDb(): Promise<IDBDatabase | null> {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise<IDBDatabase | null>((resolve) => {
    if (typeof indexedDB === "undefined") {
      if (!warnedUnavailable) {
        console.warn(
          "open-sandbox: IndexedDB unavailable — unsaved-buffer persistence disabled",
        );
        warnedUnavailable = true;
      }
      resolve(null);
      return;
    }
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE_NAME)) {
        db.createObjectStore(STORE_NAME, { keyPath: "key" });
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => {
      console.warn(
        "open-sandbox: IndexedDB open failed — unsaved-buffer persistence disabled",
        req.error,
      );
      resolve(null);
    };
    req.onblocked = () => {
      // Another tab has the DB open at a different version.
      // Resolve null and degrade gracefully rather than hang.
      console.warn("open-sandbox: IndexedDB upgrade blocked by another tab");
      resolve(null);
    };
  });
  return dbPromise;
}

function tx(
  db: IDBDatabase,
  mode: IDBTransactionMode,
): IDBObjectStore {
  return db.transaction(STORE_NAME, mode).objectStore(STORE_NAME);
}

function compositeKey(sandboxId: string, path: string): string {
  return `${sandboxId}::${path}`;
}

/** Persist a dirty buffer. Best-effort — failures are logged
 *  once and otherwise silent. */
export async function putUnsavedBuffer(
  sandboxId: string,
  path: string,
  content: string,
): Promise<void> {
  const db = await openDb();
  if (!db) return;
  await new Promise<void>((resolve) => {
    const store = tx(db, "readwrite");
    const record: BufferRecord = {
      key: compositeKey(sandboxId, path),
      sandboxId,
      path,
      content,
      savedAt: Date.now(),
    };
    const req = store.put(record);
    req.onsuccess = () => resolve();
    req.onerror = () => {
      console.warn(
        "open-sandbox: failed to persist unsaved buffer",
        req.error,
      );
      resolve();
    };
  });
}

/** Read a single persisted buffer, or null when none exists. */
export async function getUnsavedBuffer(
  sandboxId: string,
  path: string,
): Promise<BufferRecord | null> {
  const db = await openDb();
  if (!db) return null;
  return new Promise<BufferRecord | null>((resolve) => {
    const store = tx(db, "readonly");
    const req = store.get(compositeKey(sandboxId, path));
    req.onsuccess = () => resolve((req.result as BufferRecord | undefined) ?? null);
    req.onerror = () => resolve(null);
  });
}

/** Remove a persisted buffer (e.g. after a successful save). */
export async function removeUnsavedBuffer(
  sandboxId: string,
  path: string,
): Promise<void> {
  const db = await openDb();
  if (!db) return;
  await new Promise<void>((resolve) => {
    const store = tx(db, "readwrite");
    const req = store.delete(compositeKey(sandboxId, path));
    req.onsuccess = () => resolve();
    req.onerror = () => resolve();
  });
}

/** List every persisted buffer for a sandbox. Used on initial
 *  LiveEditPanel mount to offer the user a restore-prompt. */
export async function listUnsavedBuffersForSandbox(
  sandboxId: string,
): Promise<BufferRecord[]> {
  const db = await openDb();
  if (!db) return [];
  return new Promise<BufferRecord[]>((resolve) => {
    const store = tx(db, "readonly");
    const req = store.getAll();
    req.onsuccess = () => {
      const all = (req.result as BufferRecord[]) ?? [];
      resolve(all.filter((r) => r.sandboxId === sandboxId));
    };
    req.onerror = () => resolve([]);
  });
}
