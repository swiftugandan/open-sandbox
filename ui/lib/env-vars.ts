// Parses the create form's free-text env-vars textarea into the
// shape the API expects. Format: one KEY=value per line, blank lines
// and `#`-comment lines ignored. This mirrors the .env file
// convention developers already know — paste from .env and it Just
// Works. Quoting is intentionally not supported (no shell-style
// escaping), keeping behavior predictable: value is everything to
// the right of the first `=`, trimmed.

export interface ParsedEnvVars {
  vars: Record<string, string>;
  /** Lines that couldn't be parsed (no `=`, or invalid key). Surfaced
   *  inline so the user can see what was ignored before submitting. */
  invalidLines: { lineNumber: number; text: string; reason: string }[];
  /** Count of successfully parsed entries — useful for the collapsed
   *  header's "(N)" badge. */
  count: number;
}

// POSIX-ish: letters/digits/underscore, not starting with a digit.
const KEY_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

export function parseEnvVars(text: string): ParsedEnvVars {
  const vars: Record<string, string> = {};
  const invalidLines: ParsedEnvVars["invalidLines"] = [];
  const lines = text.split(/\r?\n/);
  lines.forEach((raw, i) => {
    const lineNumber = i + 1;
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) return;
    const eq = line.indexOf("=");
    if (eq === -1) {
      invalidLines.push({ lineNumber, text: raw, reason: "missing `=`" });
      return;
    }
    const key = line.slice(0, eq).trim();
    const value = line.slice(eq + 1).trim();
    if (!KEY_RE.test(key)) {
      invalidLines.push({
        lineNumber,
        text: raw,
        reason: "invalid key (letters/digits/underscore; not starting with digit)",
      });
      return;
    }
    vars[key] = value;
  });
  return { vars, invalidLines, count: Object.keys(vars).length };
}
