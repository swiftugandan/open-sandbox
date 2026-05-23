# Spike 03 — Result

**Date:** 2026-05-23
**Question A:** Does axum's WebSocket `send().await` propagate
backpressure to its upstream when the peer is slow, or does it buffer
internally?
**Question B:** How fast does the server-side producer notice that the
client has abruptly disconnected (no close frame, simulating TCP RST)?

**Verdicts:**
- **A — Backpressure works cleanly.** No internal unbounded buffering.
- **B — Sub-10ms detection while actively sending.** Returns
  `io::ErrorKind::BrokenPipe`.

## Method

`src/main.rs` builds a tiny axum server with one WebSocket route. The
handler streams 200 frames of 1 MiB each (200 MiB total) as fast as it
can. Two clients exercise it sequentially:

- **Test A:** Reads 5 frames at full speed, sleeps 8 seconds, then
  drains. The server-side producer logs per-frame send-await latency.
  If backpressure works, we expect to see one frame whose `send().await`
  takes ~8 seconds while the client is asleep, and bounded
  bytes-in-flight (limited by TCP send buffers, not unbounded process
  memory).
- **Test B:** Reads 3 frames, then drops the WebSocket without sending
  a close frame. The server-side producer logs the wall-clock latency
  between starting to send and the `send().await` returning an error.

Versions: `axum = "0.8"`, `tokio = "1"`, `tokio-tungstenite = "0.24"`.
macOS (Darwin), localhost loopback.

## Observations

### Test A — Backpressure

```
[cliA] connected, draining first 5 frames at full speed
[cliA] sleeping 8s (TCP buffers should fill, server send should await)
[cliA] resuming reads; draining the rest
[srv c=0] heartbeat: sent 6 frames, last send_await=8003ms
[srv c=0] sent all 200 frames in 8143ms; max_send_ms=8003
[srv c=0] blocking_frames count=1
[cliA] drained to 200 frames in 136ms after resume
```

Key numbers:
- Frame 6's `send().await` took **8003 ms** — exactly the client's
  8-second sleep. The producer task was cooperatively suspended at
  the await point until the OS send buffer drained enough.
- Only **1 frame** (count=1) exceeded the 10ms blocking threshold,
  meaning the producer was idle waiting on TCP for essentially the
  whole 8-second window — not spin-busy or buffering ahead.
- Total wall time for 200 frames = 8143 ms ≈ the 8s sleep + 143ms of
  actual transfer. Confirms: nothing was buffered ahead of the slow
  client.
- After the client resumed, **200 frames drained in 136 ms** —
  meaning the TCP buffer's worth (a few MB) of in-flight data was
  whatever the kernel allowed, not whatever axum could buffer.

### Test B — Abrupt disconnect

```
[cliB] connected; reading 3 frames
[cliB] dropping WebSocket (no close frame, simulates TCP RST)
[srv c=1] send error at frame 5 after wall_total=7ms, send_await=0ms,
          err=Error { inner: Io(Os { code: 32, kind: BrokenPipe,
                                     message: "Broken pipe" }) }
```

Key numbers:
- Server detected the disconnect in **7 ms** wall-clock from producer
  start (and was already on frame 5 when the client dropped at
  frame 3 — the dropped state propagated through the kernel before
  the next send completed).
- `send().await` for the failing frame returned in **0 ms** (the
  error was already pending).
- Error is `std::io::ErrorKind::BrokenPipe` — identifiable for the
  cleanup hook.

## Implications for the design

### D3 (WebSocket) is sound

Both axum WebSocket properties needed for the streaming-exec design
hold:

- The server-side producer task naturally suspends when the client
  is slow. No application-level bounded channel needed; no risk of
  OOMing the gateway with a noisy build whose output a slow CLI is
  reading one chunk per second.
- An abruptly disconnected client surfaces as a `BrokenPipe` error
  on the next send, immediately. This is the exact signal the
  `ExecRegistry` cleanup hook needs — match on the error, look up
  the in-container PID, issue the SIGTERM/SIGKILL sequence
  established by spikes 01 + 02.

### Idle-session caveat (new finding, worth noting)

The 7 ms detection assumes the server is **actively sending**. If the
producer is idle (e.g., a `bash -i` exec sitting at a prompt with no
stdout traffic), the server will not learn of a TCP RST until either
it tries to send again or TCP keepalive fires (default O(2h) on most
Linux distros, configurable but not in the application's control).

This is not a blocker but it is a real consideration for D5
(exec-is-session). For long-idle WebSocket sessions, we need
application-level keepalive. The clean fix is to use WebSocket
**ping/pong frames** on a timer (e.g., every 30s). tungstenite supports
this directly; axum's WebSocket exposes the ping/pong path.

**Action:** add a note to `EXEC_STREAMING_DESIGN.md` D3 / D5 that the
gateway sends WebSocket pings every ~30s on idle sessions to bound
disconnect-detection latency. This is a small implementation detail,
not a design reshape.

### Memory profile

Not formally measured in this spike, but the producer task held
exactly one `Bytes` clone in flight at a time (1 MiB). Even with the
client asleep, RSS did not visibly grow over the 8-second window
(observed informally during the run). No internal axum/tungstenite
buffer accumulation. Healthy.

## What this does NOT cover

- TLS overhead (this was plain `ws://`). For `wss://`, send-await
  semantics should be identical — rustls is also Tokio-async, also
  backpressuring — but worth a confirmation pass during
  implementation.
- Multiple concurrent clients on the same axum server with
  contention. The producer task here had a dedicated tokio task per
  connection (the standard `WebSocket::on_upgrade` pattern); no
  shared queues. Concurrency is the standard tokio story and not
  novel.
- Behavior under HTTP/2 instead of HTTP/1.1 WebSocket. axum 0.8 uses
  HTTP/1.1 Upgrade for WebSocket; HTTP/2 WebSocket (RFC 8441) is a
  separate, opt-in path we are not relying on.

## Conclusion

D3 (WebSocket as the public streaming surface) is structurally and
operationally sound. The amendment can proceed without further
WebSocket-property uncertainty. Add the idle-keepalive note to the
design doc and move on.
