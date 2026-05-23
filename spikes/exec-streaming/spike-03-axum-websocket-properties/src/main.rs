//! Spike 03 — axum WebSocket: does send() naturally backpressure when the
//! peer is slow, and how fast does the server notice a TCP-abrupt
//! disconnect?
//!
//! Two tests run sequentially against the same axum server:
//!
//!   A. Backpressure: client reads 5 frames at full speed, sleeps 8s, then
//!      drains. If `send().await` is doing the right thing, we expect to
//!      see per-frame send latency spike up while the client is asleep
//!      (because TCP send buffers are full), and total bytes-in-flight to
//!      stay bounded around the kernel buffer size.
//!
//!   B. Abrupt disconnect: client reads 3 frames, then drops the
//!      WebSocket without sending a close frame (simulating TCP RST /
//!      crash). Measure how long the server-side producer takes to learn.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;

const FRAME_BYTES: usize = 1024 * 1024; // 1 MiB
const TOTAL_FRAMES: usize = 200;

static CONN_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/ws");

    let app = Router::new().route("/ws", get(handler));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    println!("\n=== Test A: backpressure under slow consumer ===\n");
    test_a_backpressure(&url).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("\n=== Test B: abrupt client disconnect detection ===\n");
    test_b_disconnect(&url).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    server.abort();
}

async fn handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(producer)
}

async fn producer(mut socket: WebSocket) {
    let id = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let payload: bytes::Bytes = vec![0xABu8; FRAME_BYTES].into();
    let started = Instant::now();
    let mut last_log = Instant::now();
    let mut max_send_ms: u128 = 0;
    let mut blocking_frames: Vec<(usize, u128)> = Vec::new();

    eprintln!("[srv c={id}] producer started, frame_bytes={FRAME_BYTES}, total={TOTAL_FRAMES}");

    for i in 0..TOTAL_FRAMES {
        let t0 = Instant::now();
        let res = socket.send(Message::Binary(payload.clone())).await;
        let elapsed = t0.elapsed();
        let ms = elapsed.as_millis();
        max_send_ms = max_send_ms.max(ms);
        if ms > 10 {
            blocking_frames.push((i, ms));
        }

        if let Err(e) = res {
            let total = started.elapsed().as_millis();
            eprintln!(
                "[srv c={id}] send error at frame {i} after wall_total={total}ms, send_await={ms}ms, err={e:?}"
            );
            eprintln!("[srv c={id}] max_send_ms={max_send_ms}, blocking_frames={blocking_frames:?}");
            return;
        }

        if last_log.elapsed() > Duration::from_secs(1) {
            eprintln!("[srv c={id}] heartbeat: sent {i} frames, last send_await={ms}ms");
            last_log = Instant::now();
        }
    }
    eprintln!(
        "[srv c={id}] sent all {TOTAL_FRAMES} frames in {}ms; max_send_ms={max_send_ms}",
        started.elapsed().as_millis()
    );
    eprintln!("[srv c={id}] blocking_frames count={}", blocking_frames.len());
}

async fn test_a_backpressure(url: &str) {
    let (mut ws, _) = connect_async(url).await.expect("connect A");
    eprintln!("[cliA] connected, draining first 5 frames at full speed");
    for _ in 0..5 {
        let _ = ws.next().await;
    }
    eprintln!("[cliA] sleeping 8s (TCP buffers should fill, server send should await)");
    tokio::time::sleep(Duration::from_secs(8)).await;
    eprintln!("[cliA] resuming reads; draining the rest");
    let t0 = Instant::now();
    let mut got = 5usize;
    while let Some(frame) = ws.next().await {
        match frame {
            Ok(_) => {
                got += 1;
                if got >= TOTAL_FRAMES {
                    break;
                }
            }
            Err(e) => {
                eprintln!("[cliA] read error after {got} frames: {e:?}");
                break;
            }
        }
    }
    eprintln!(
        "[cliA] drained to {got} frames in {}ms after resume",
        t0.elapsed().as_millis()
    );
}

async fn test_b_disconnect(url: &str) {
    let (mut ws, _) = connect_async(url).await.expect("connect B");
    eprintln!("[cliB] connected; reading 3 frames");
    for _ in 0..3 {
        let _ = ws.next().await;
    }
    let drop_at = Instant::now();
    eprintln!("[cliB] dropping WebSocket (no close frame, simulates TCP RST)");
    drop(ws);

    // The server-side producer logs when send() returns Err. We measure
    // the wall time between drop_at and that error log by sleeping and
    // checking; the producer task's stderr captures the actual point.
    // For the spike we just give it a generous window.
    tokio::time::sleep(Duration::from_secs(10)).await;
    eprintln!(
        "[cliB] {}s wall-clock since drop — see [srv c=1] line for server-detected latency",
        drop_at.elapsed().as_secs()
    );
}
