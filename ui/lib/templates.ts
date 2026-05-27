// Quickstart templates surfaced in the create form. Each entry pairs
// an image with the exec command that would make the sandbox's public
// URL serve something useful — so "Create → Run → click URL" works on
// first try instead of dead-ending at an idle container.
//
// The `execCommand` is the literal string the exec terminal would
// expect (shell-style; parsed by `parseCommand` in ui/lib/api.ts).
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
    execCommand:
      'sh -c "cd /tmp && echo \'<h1>hello from open-sandbox</h1>\' > index.html && python3 -m http.server 8080"',
    exposedPort: 8080,
    description: "python http.server on :8080 — visit the URL, see hello.",
  },
  {
    id: "node",
    label: "Node",
    image: "node:20-alpine",
    execCommand:
      "node -e \"require('http').createServer((q,r)=>r.end('hi from node')).listen(8080)\"",
    exposedPort: 8080,
    description: "One-line Node HTTP server on :8080.",
  },
  {
    id: "nginx",
    label: "Nginx",
    image: "nginx:alpine",
    // nginx:alpine listens on :80 by default; rewrite to :8080 so the
    // proxy's default exposed-port wiring picks it up without the
    // create form having to configure exposed_port=80.
    execCommand:
      "sh -c \"sed -i 's/listen       80;/listen       8080;/' /etc/nginx/conf.d/default.conf && nginx -g 'daemon off;'\"",
    exposedPort: 8080,
    description: "Stock nginx welcome page on :8080.",
  },
  {
    id: "python-flask",
    label: "Flask",
    image: "python:3.12-alpine",
    // Outer single quotes so the dev-console's parseCommand (which has
    // no backslash-escape support) doesn't have to unwind nested
    // quotes. Inside, the python script uses double quotes and the
    // shell's own `\"` escape — sh interprets that, not us.
    execCommand:
      `sh -c 'pip install --quiet flask && python3 -c "from flask import Flask; a=Flask(__name__); a.route(\\"/\\")(lambda: \\"hi from flask\\"); a.run(host=\\"0.0.0.0\\", port=8080)"'`,
    exposedPort: 8080,
    description: "pip install flask, serve one route on :8080.",
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
