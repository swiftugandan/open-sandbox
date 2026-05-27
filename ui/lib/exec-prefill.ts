// Single-shot hand-off from the create form to the next ExecTerminal
// that mounts for a given sandbox_id. The terminal reads, then clears,
// so the value is consumed once and the user owns the input from
// that point forward.
//
// Carries:
//   - `command`: the exec line to prefill
//   - `autorun`: whether the terminal should fire run() as soon as
//     the sandbox status flips to "running". Set by the create form
//     when the user chose a template AND didn't override the image
//     (a custom image is likelier to be missing whatever binary the
//     template's exec command assumes — let the user inspect first).

const PREFIX = "open-sandbox:exec:";

export interface ExecPrefill {
  command: string;
  autorun: boolean;
}

export function stashExecPrefill(
  sandboxId: string,
  command: string,
  opts: { autorun?: boolean } = {},
): void {
  if (!command) return;
  try {
    const payload: ExecPrefill = {
      command,
      autorun: opts.autorun ?? false,
    };
    window.sessionStorage.setItem(PREFIX + sandboxId, JSON.stringify(payload));
  } catch {
    // Storage may be unavailable (private mode quirks, disabled
    // storage, …). Prefill is purely a UX nicety — failing it
    // silently is the right call.
  }
}

export function consumeExecPrefill(sandboxId: string): ExecPrefill | null {
  try {
    const raw = window.sessionStorage.getItem(PREFIX + sandboxId);
    if (raw === null) return null;
    window.sessionStorage.removeItem(PREFIX + sandboxId);
    const parsed = JSON.parse(raw) as Partial<ExecPrefill>;
    if (typeof parsed.command !== "string") return null;
    return { command: parsed.command, autorun: Boolean(parsed.autorun) };
  } catch {
    return null;
  }
}
