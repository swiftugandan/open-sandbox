# e2e scenarios

Live verification of the v1.0 streaming-exec data plane. Each
script in this directory is a self-contained scenario that:

1. Creates a sandbox via the REST API.
2. Waits for it to enter `running` state.
3. Waits for the proxy's routing cache to refresh (~32s).
4. Exercises one specific property of the streaming exec / file
   ops surface.
5. Tears the sandbox back down via the trap on `EXIT`.

## Running

```bash
# Bring up the full stack: postgres, controller, proxy, api, agent.
docker compose -f infra/e2e/docker-compose.full.yml up -d

# Build the reference clients (the scenarios use opensandbox-exec).
cargo build --release -p open-sandbox-ws-client

# Run every scenario, report PASS/FAIL summary at the end.
infra/e2e/scenarios/run-all.sh
```

Exit code = number of failing scenarios.

## Scenarios

| #   | Script                  | What it verifies                                                                                        |
|-----|-------------------------|---------------------------------------------------------------------------------------------------------|
| 01  | `01-echo.sh`            | The minimum signal-of-life: `echo hello` round-trips and exits 0.                                       |
| 02  | `02-bulk-transfer.sh`   | 10 MiB of stdout streams intact end-to-end (backpressure works; no truncation).                          |
| 03  | `03-signal-term.sh`     | `kill -TERM` against the in-container PID actually delivers a signal — the trap fires + exit 143.       |
| 04  | `04-disconnect-kills.sh`| Client `SIGKILL` triggers the agent's `ExecRegistry` cleanup → SIGTERM/SIGKILL inside the container.    |
| 05  | `05-idle-keepalive.sh`  | A 90s idle WebSocket session survives the 30s ping/60s pong window without being torn down.             |
| 06  | `06-long-running.sh`    | `sleep 70` completes cleanly — v1.0 sessions have no built-in per-call timeout; the connection IS the lifetime. |
| 07  | `07-command-not-found.sh` | An unknown binary exits 127 AND the `command_not_found` flag is surfaced (cnf marker on stderr).      |
| 08  | `08-write-then-exec.sh` | `write_file` (single-file REST) followed by exec of the uploaded script works end-to-end.               |
| 09  | `09-ws-read-file.sh`    | Streaming `WS /files/read-stream`: a 256 KiB payload round-trips intact via the WebSocket variant.       |

For the SDK-level introduction to the same surface, see the
runnable examples in `crates/ws-client/examples/`.

## Helpers

`common.sh` defines the shared bash helpers (sandbox lifecycle,
`wait_for_routing`, log/pass/fail). Keep it dependency-free —
the scenarios should run on any developer machine with bash,
curl, jq, and the docker-compose stack up.
