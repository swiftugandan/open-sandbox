/** v1.0.3: client-side exclude patterns the file tree hides by default.
 *
 *  Pure UI policy — the gateway's list_dir endpoint returns ALL entries
 *  (capped at 5000 server-side); this filter only controls what the
 *  tree renders. Toggle visibility with `Cmd-Shift-H`.
 *
 *  The list intentionally stays short: only directory names that
 *  routinely contain thousands of generated files and that no user
 *  would want to expand by accident. Source-controlled config dirs
 *  (e.g. `.github/`, `.vscode/`) stay visible — they're small and the
 *  user might legitimately want to edit them. */
export const DEFAULT_HIDDEN_DIRS: ReadonlySet<string> = new Set([
  "node_modules",
  ".git",
  "target",
  "dist",
  "__pycache__",
  ".next",
  ".venv",
  ".turbo",
  ".cache",
]);

/** Predicate convenience: should this entry name be hidden by default? */
export function isHiddenByDefault(name: string): boolean {
  return DEFAULT_HIDDEN_DIRS.has(name);
}
