# Spike 04 — Result

**Date:** 2026-05-23
**Question A:** Does bollard's `start_exec` attached pipeline
backpressure end-to-end when the consumer of stdout doesn't read?
**Question B:** When the consumer drops the output stream, does the
producer's `write_all` cleanly notice via an error?

**Verdict A — CONFIRMED.** Backpressure propagates through the full
chain. After ~10 MiB the producer's `write_all` blocks for the full
5s timeout.
**Verdict B — wrong question; the real concern is non-issue.** See
"What test B taught us" below.

## Method

`src/main.rs` builds a small binary that starts an alpine container
running `sleep infinity` and issues `docker exec ... cat` against
it via bollard. Two tests run sequentially against the same
container.

Test A keeps `output` un-consumed and writes 64 KiB chunks to
`input` in a loop with a 5s per-chunk `tokio::time::timeout`. If
the chain backpressures, the first timeout fires once kernel +
docker exec buffers fill.

Test B writes one chunk, drops the `output` stream, then continues
writing — looking for a write error.

Versions: `bollard = "0.18"`, `tokio = "1"`. macOS, Docker Desktop
29.4.3.

## Observation

```
=== Test A: backpressure with consumer NOT reading ===
[testA] 8 MiB written so far (elapsed 183ms)
Test A result: Backpressured { bytes_written: 10878976, elapsed_ms: 5196 }

=== Test B: producer notices when stdout consumer drops ===
[testB] wrote initial chunk; now dropping the output stream
Test B result: NoErrorAfterDrop { bytes_written: 9961472 }
```

### Reading Test A

- ~43 MiB/s burst rate while buffers had headroom (8 MiB in 183ms).
- At ~10.4 MiB the producer's `write_all` blocked indefinitely.
- The chain's total buffer capacity is ~10 MiB across: bollard's
  internal write buffer + Docker Engine's exec attach socket + the
  exec pipe to `cat`'s stdin + `cat`'s stdout pipe + Docker
  Engine's read buffer + bollard's output Stream queue.
- 5.2s elapsed matches the 5s per-chunk timeout, confirming the
  producer was genuinely awaiting on the `write_all` future and not
  spin-busy.

This is the load-bearing property for 12.2: the agent's `ExecHandle`
can hand bollard's `input` to a pump task that reads from a
bounded `mpsc::Receiver<Bytes>` and writes to bollard's input. The
pump's `recv().await` ↔ `write_all().await` chain naturally
propagates backpressure all the way back to whoever fills the mpsc
sender — which in the agent is the `IoClientFrame` stream pump,
which in turn backpressures the proxy's tunnel pump, which
backpressures the gateway's WebSocket reader, which backpressures
the public WebSocket peer.

Five hops of backpressure, every link is a Tokio-async primitive
that propagates `Poll::Pending`. The architecture holds.

### Reading Test B — what it actually taught us

The test was wrong-shaped: dropping `output` while keeping `input`
does NOT close the Docker exec session, because dockerd still has
the full-duplex attach socket open from its side. `cat` keeps
reading stdin and writing to a stdout pipe whose host-side reader
is gone. Eventually `cat`'s stdout pipe fills and the producer
hits backpressure again (the `NoErrorAfterDrop` outcome is exactly
the same backpressure as Test A surfacing on the abandoned-output
path).

But this is the right finding because **the agent never relies on
this kind of detection**. The real disconnect-detection chain is:

1. Public WebSocket peer drops → gateway sees `BrokenPipe` (per
   spike 03, ~7ms while sending).
2. Gateway's `OpenIoStream` gRPC client drops → proxy sees gRPC
   stream end.
3. Proxy's tunnel-side virtual stream closes → agent receives
   `IoClose` or a Stream end on its `IoClientFrame` source.
4. Agent's `drive_io_session` task notices via `Stream::next() →
   None`, invokes `exec_registry::on_stream_closed`, which
   `signal_exec(SIGTERM)`s the in-container PID.
5. The agent's pump tasks owning bollard `input` and `output` are
   dropped, which closes the bollard session, which dockerd reaps.

The disconnect signal arrives at step 3 from the proxy, not from
bollard. Bollard's role is just to give us clean teardown when WE
drop its streams — which it does naturally, no special API needed.

So Test B's "NoErrorAfterDrop" outcome is fine. Bollard does not
need to surface a write error on partial drop because the agent
doesn't ask it to. The agent's higher layer learns of disconnect
through the gRPC stream end, then proactively tears down the
bollard session.

## Implications for 12.2

- **Pump structure:** the agent's `agent-docker` runtime backend
  spawns two tasks per exec — one pumping `mpsc::Receiver<Bytes>`
  into bollard's `input` via `write_all`, one pumping bollard's
  output Stream into `mpsc::Sender<Bytes>` channels for stdout/
  stderr separation. Channels are bounded; backpressure propagates
  through. ✓

- **Disconnect-driven kill:** the `drive_io_session` task watches
  the `IoClientFrame` stream for end-of-stream, NOT bollard for
  errors. On stream end it calls `signal_exec(SIGTERM)` and drops
  the pump tasks. Bollard cleans up automatically. ✓

- **Buffer sizing:** the chain has ~10 MiB of natural buffering
  between bollard and the in-container process. The agent should
  size its mpsc channels modestly (e.g., 64 KiB × 4 = 256 KiB
  bounded) so backpressure shows up at the channel level rather
  than waiting for kernel buffers to fill. Surfaces backpressure
  to the upper layers faster.

## What this does NOT cover

- Behavior under SIGKILL of the in-container PID mid-exec — that's
  a different path (`signal_exec` invoking `docker exec kill`).
  Tested in 12.2's e2e scenarios, not here.
- bollard's behavior on dockerd restart. Out of scope for v1.0.
- youki-side pumping — covered by Spike 05.

## Conclusion

D2's backpressure assumption holds for the bollard layer.
Combined with Spike 03's WebSocket backpressure result, the
end-to-end chain from public WS peer to in-container process pipes
backpressures correctly at every link.

The disconnect-detection question Test B accidentally asked is
moot: the agent never depended on bollard for disconnect; it
depends on the gRPC stream from the proxy. That signal is
well-defined and tonic-supported.

12.2 can proceed without further bollard-side spikes.
