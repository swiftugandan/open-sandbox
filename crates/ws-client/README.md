# open-sandbox-ws-client

Rust SDK for the Open Sandbox v1.0 streaming exec WebSocket API.

## What this crate gives you

- An `ExecSession` type that opens a bidirectional exec stream
  against a sandbox over WebSocket, with stdin/stdout/stderr,
  signals, and structured terminal frames.
- A reference CLI (`opensandbox-exec`) that wraps the SDK for
  one-shot command execution from a shell prompt.
- Three example binaries (`examples/`) that demonstrate the
  shape of typical agent flows.

This is the first-party reference client for the wire protocol
defined in `crates/contracts`. The SDK is intentionally thin —
all the protocol-specific knowledge lives in
`crates/contracts/proto/proxy.proto` and is decoded one frame at
a time.

## Wire shape (one paragraph)

The session is a single WebSocket carrying binary frames in a
`| 1 byte kind | payload |` envelope:

| Direction | Kind   | Payload                             |
|-----------|--------|-------------------------------------|
| C → S     | `0x00` | `IoStart` (proto) — sent first      |
| C → S     | `0x01` | raw stdin bytes                     |
| C → S     | `0x02` | `IoSignal` (signum)                 |
| C → S     | `0x03` | empty — half-close stdin            |
| S → C     | `0x11` | raw stdout bytes                    |
| S → C     | `0x12` | raw stderr bytes                    |
| S → C     | `0x13` | `IoExited` — terminal               |
| S → C     | `0x14` | `IoError` — terminal                |
| S → C     | `0x15` | `IoStarted` — pid + exec id         |

`Authorization: Bearer <api-key>` is required on the WebSocket
upgrade. The connection is the lifetime of the exec: closing the
WebSocket triggers `SIGTERM` (5s grace) then `SIGKILL` on the
in-container PID.

## Minimal usage

```rust
use open_sandbox_ws_client::{ExecParams, ExecSession, ServerFrame};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut session = ExecSession::connect(
        "ws://localhost:8081",
        "00000000-0000-0000-0000-000000000001",
        "my-api-key",
        ExecParams::new(vec!["echo".into(), "hello".into()]),
    )
    .await?;

    while let Some(frame) = session.next_frame().await? {
        match frame {
            ServerFrame::Stdout(bytes) => {
                print!("{}", String::from_utf8_lossy(&bytes));
            }
            ServerFrame::Stderr(bytes) => {
                eprint!("{}", String::from_utf8_lossy(&bytes));
            }
            ServerFrame::Exited { exit_code, .. } => {
                std::process::exit(exit_code);
            }
            ServerFrame::Error { code, detail } => {
                eprintln!("error: {code}: {detail}");
                std::process::exit(1);
            }
            ServerFrame::Started { .. } => {}
        }
    }
    Ok(())
}
```

## Examples

Three runnable examples are included under `examples/`:

| Example              | What it shows                                              |
|----------------------|------------------------------------------------------------|
| `echo`               | Minimal command + capture stdout                           |
| `long-running-build` | Exec runs past the legacy 60s timeout (no client timeout)  |
| `interactive-bash`   | Bidirectional shell; half-close stdin to signal EOF        |
| `stream-read-file`   | Stream a file out via `WS /files/read-stream`              |

Run them against a running stack:

```bash
docker compose -f infra/e2e/docker-compose.full.yml up -d
SB=$(curl -s -X POST http://localhost:18081/v1/sandboxes \
       -H 'Authorization: Bearer e2e-api-key' \
       -H 'content-type: application/json' \
       -d '{"image":"alpine"}' | jq -r .sandbox_id)
# Wait ~32s for the proxy routing cache to refresh.
cargo run -p open-sandbox-ws-client --example echo \
  -- --sandbox "$SB"
cargo run -p open-sandbox-ws-client --example long-running-build \
  -- --sandbox "$SB"
cargo run -p open-sandbox-ws-client --example interactive-bash \
  -- --sandbox "$SB" --once
```

## CLI: `opensandbox-exec`

The `opensandbox-exec` binary is a thin shell wrapper around
`ExecSession`. Useful for poking at a sandbox from a terminal:

```bash
opensandbox-exec \
  --base ws://localhost:18081 \
  --sandbox "$SB" \
  --api-key e2e-api-key \
  -- ls -la /tmp
```

It exits with the in-container process's exit code. On
`command_not_found` it prints `# command not found` to stderr.
