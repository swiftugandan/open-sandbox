/** v1.0.3 live-edit: lazy CodeMirror language-pack loader.
 *
 *  Each `@codemirror/lang-*` package lands in its own Next.js
 *  chunk via `import()` so a `.py` file open doesn't drag the JS
 *  parser into the initial bundle. Per the PLAN_LIVE_EDIT spike,
 *  `@codemirror/lang-python` lands in a ~27KB gzip chunk; the
 *  core editor + basicSetup is ~165KB gzip.
 *
 *  Returns the CodeMirror `Extension` array for the chosen
 *  language, or an empty array when the file extension doesn't
 *  map to a known pack (no syntax highlighting; the editor still
 *  works as a plain text editor). */

import type { Extension } from "@codemirror/state";

/** Map a file path / name to its language id. The extension is
 *  the source of truth — content-sniffing is out of scope. */
export function languageIdFor(path: string): LanguageId | null {
  const dot = path.lastIndexOf(".");
  if (dot < 0 || dot === path.length - 1) return null;
  const ext = path.slice(dot + 1).toLowerCase();
  switch (ext) {
    case "js":
    case "jsx":
    case "mjs":
    case "cjs":
      return "javascript";
    case "ts":
      return "typescript";
    case "tsx":
      return "tsx";
    case "py":
    case "pyi":
      return "python";
    default:
      return null;
  }
}

export type LanguageId = "javascript" | "typescript" | "tsx" | "python";

/** Dynamically load the CodeMirror language extension for `id`.
 *  Resolves with an `Extension[]` ready to drop into the editor's
 *  `extensions` prop. */
export async function loadLanguage(id: LanguageId): Promise<Extension[]> {
  switch (id) {
    case "javascript": {
      const { javascript } = await import("@codemirror/lang-javascript");
      return [javascript({ jsx: true })];
    }
    case "typescript": {
      const { javascript } = await import("@codemirror/lang-javascript");
      return [javascript({ typescript: true })];
    }
    case "tsx": {
      const { javascript } = await import("@codemirror/lang-javascript");
      return [javascript({ typescript: true, jsx: true })];
    }
    case "python": {
      const { python } = await import("@codemirror/lang-python");
      return [python()];
    }
  }
}
