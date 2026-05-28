// Quickstart templates surfaced in the create form. Each entry pairs
// an image with the exec command that would make the sandbox's public
// URL serve something useful — so "Create → Run → click URL" works on
// first try instead of dead-ending at an idle container.
//
// **/workspace convention.** Every template seeds files into
// `/workspace` and serves from there. That folder is the Edit tab's
// default tree root (see `DEFAULT_TREE_ROOT` in
// `ui/components/file-tree.tsx`), so the source the user is editing
// is also the source the public URL is serving. None of these base
// images ship `/workspace`, so each command starts with
// `mkdir -p /workspace`.
//
// The `execCommand` is the literal string the exec terminal would
// expect (shell-style; parsed by `parseCommand` in ui/lib/api.ts).
// `parseCommand` only understands quote pairs (no backslash escapes),
// so multi-line / embedded-quote payloads are written via `printf`
// with shell-escaped `\"` rather than heredocs.
//
// `exposedPort` documents what the in-container process listens on;
// the API today only honors a non-zero value here for routing, so
// templates whose port differs from the platform default (8080)
// should override it.

export interface Template {
  id: string;
  label: string;
  image: string;
  // Empty string means "no auto-exec" — the plain-shell template
  // surfaces an empty terminal prompt rather than overriding the
  // exec-terminal's default placeholder.
  execCommand: string;
  exposedPort: number;
  description: string;
}

export const TEMPLATES: readonly Template[] = [
  {
    id: "static-site",
    label: "Static site",
    image: "python:3.12-alpine",
    // Seed /workspace/index.html, then serve /workspace on :8080.
    // python -m http.server uses cwd as the doc root.
    //
    // `exec` on the final command replaces the wrapping sh, so the
    // exec session's recorded PID points at python3 directly. A
    // SIGTERM from the agent on WS disconnect (spike-01 / ADR-006)
    // then reaches python3 instead of an outer sh whose dying-child
    // (python3) would be orphaned to PID 1 and survive.
    execCommand:
      'sh -c "mkdir -p /workspace && cd /workspace && echo \'<h1>hello from open-sandbox — edit /workspace/index.html</h1>\' > index.html && exec python3 -m http.server 8080"',
    exposedPort: 8080,
    description:
      "python http.server serving /workspace on :8080. Edit /workspace/index.html.",
  },
  {
    id: "node",
    label: "Node",
    image: "node:20-alpine",
    // Outer single quotes so parseCommand passes the inner `"…"`
    // payload to sh as one token. printf writes a two-line server.js
    // into /workspace via `\"` shell-escapes (parseCommand sees them
    // as literal text since they live inside single quotes; sh then
    // interprets them).
    //
    // `node --watch` (stable since Node 20) restarts the process
    // when any required source file changes — so saving server.js
    // in the Edit tab reloads the server on the next request.
    //
    // `exec` on the final command replaces sh with node, so SIGTERM
    // from the agent reaches the watcher directly; node --watch then
    // cleans up its child fork on shutdown.
    execCommand:
      `sh -c 'mkdir -p /workspace && cd /workspace && printf "%s\\n" "const http = require(\\"http\\");" "http.createServer((q, r) => r.end(\\"hi from node — edit /workspace/server.js\\")).listen(8080);" > server.js && exec node --watch server.js'`,
    exposedPort: 8080,
    description:
      "Node HTTP server on :8080 with --watch. Edit /workspace/server.js; save reloads.",
  },
  {
    id: "nginx",
    label: "Nginx",
    image: "nginx:alpine",
    // nginx:alpine listens on :80 and serves /usr/share/nginx/html
    // by default. Two sed rewrites make it listen on :8080 (so the
    // proxy's default port wiring catches it) and serve /workspace
    // (so the Edit tab and the public URL stay aligned).
    execCommand:
      "sh -c \"mkdir -p /workspace && echo '<h1>hello from nginx — edit /workspace/index.html</h1>' > /workspace/index.html && sed -i 's|listen       80;|listen       8080;|; s|/usr/share/nginx/html|/workspace|' /etc/nginx/conf.d/default.conf && exec nginx -g 'daemon off;'\"",
    exposedPort: 8080,
    description:
      "nginx serving /workspace on :8080. Edit /workspace/index.html.",
  },
  {
    id: "python-flask",
    label: "Flask",
    image: "python:3.12-alpine",
    // Same `printf "..." "..."` shape as the node template: outer
    // single quotes for parseCommand, inner `\"` escapes consumed by
    // sh. Writes /workspace/app.py, then pip-installs Flask and runs
    // the script.
    //
    // `use_reloader=True, use_debugger=False` enables Werkzeug's
    // stat-based reloader (so saving app.py reloads the route on
    // the next request) WITHOUT enabling Werkzeug's interactive
    // debugger. The debugger would expose source tracebacks and a
    // PIN-gated arbitrary-Python-eval console over the public URL —
    // PINs are deterministic from machine state and brute-forceable
    // in seconds inside a known image. Splitting the two flags
    // gives the user the dev ergonomics without the RCE surface.
    //
    // `exec` on the final command replaces sh with python3 so the
    // agent's SIGTERM reaches the reloader parent directly; the
    // parent then takes the reloader child down on its way out.
    execCommand:
      `sh -c 'mkdir -p /workspace && cd /workspace && printf "%s\\n" "from flask import Flask" "a = Flask(__name__)" "@a.route(\\"/\\")" "def hi(): return \\"hi from flask — edit /workspace/app.py\\"" "a.run(host=\\"0.0.0.0\\", port=8080, use_reloader=True, use_debugger=False)" > app.py && pip install --quiet flask && exec python3 app.py'`,
    exposedPort: 8080,
    description:
      "Flask app on :8080 with reloader. Edit /workspace/app.py; save reloads.",
  },
  {
    id: "shell",
    label: "Plain shell",
    image: "alpine:3.21",
    execCommand: "",
    exposedPort: 0,
    description: "Bare alpine container. Type your own command.",
  },
] as const;

export const DEFAULT_TEMPLATE_ID = "static-site";

// The bare-container entry exists in TEMPLATES so the rest of the
// code can resolve it by id, but it's surfaced through its own
// "Blank sandbox" button rather than the dropdown — pre-baked
// templates all promise "click the URL, see something"; the blank
// path promises nothing and would teach users the dropdown is
// hit-or-miss.
export const BLANK_TEMPLATE_ID = "shell";

export const DROPDOWN_TEMPLATES: readonly Template[] = TEMPLATES.filter(
  (t) => t.id !== BLANK_TEMPLATE_ID,
);

export function findTemplate(id: string): Template | undefined {
  return TEMPLATES.find((t) => t.id === id);
}
