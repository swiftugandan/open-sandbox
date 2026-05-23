//! Spike 04 — bollard concurrent stdin/stdout pumping under cancellation.
//!
//! Two questions:
//!
//!   A. Backpressure — when the consumer of stdout doesn't read, does
//!      writing more stdin eventually block (await), or does bollard
//!      buffer unboundedly?
//!
//!   B. Drop / cancellation — when the consumer side is dropped, does
//!      the producer's `write_all` cleanly error, signalling to the
//!      agent-side state machine that the session is gone?
//!
//! The setup uses `docker exec <ctr> cat` — cat echoes stdin to
//! stdout. With the consumer not reading, cat's stdout pipe fills,
//! cat stops reading stdin, the bollard input pipe stops draining,
//! and `input.write_all` should await. End-to-end backpressure
//! through:
//!
//!     producer task → bollard input AsyncWrite → docker exec stdin
//!       → cat process stdin → cat process stdout → docker exec stdout
//!       → bollard output Stream → consumer task
//!
//! Verifies the whole chain backpressures correctly, which is the
//! load-bearing property for 12.2 streaming exec.

use std::time::{Duration, Instant};

use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

const CTR_NAME: &str = "spike04-bollard";
const CHUNK_BYTES: usize = 64 * 1024;
const TOTAL_TO_WRITE_BYTES: usize = 100 * 1024 * 1024; // 100 MiB
const WRITE_TIMEOUT_PER_CHUNK: Duration = Duration::from_secs(5);

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let docker = Docker::connect_with_local_defaults()?;

    cleanup(&docker).await;
    create_container(&docker).await?;

    println!("\n=== Test A: backpressure with consumer NOT reading ===\n");
    let result_a = test_backpressure(&docker).await;
    println!("Test A result: {result_a:?}\n");

    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("\n=== Test B: producer notices when stdout consumer drops ===\n");
    let result_b = test_drop(&docker).await;
    println!("Test B result: {result_b:?}\n");

    cleanup(&docker).await;
    Ok(())
}

async fn cleanup(docker: &Docker) {
    let _ = docker
        .remove_container(
            CTR_NAME,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

async fn create_container(docker: &Docker) -> Result<(), Box<dyn std::error::Error>> {
    // Ensure alpine is pulled.
    use bollard::image::CreateImageOptions;
    let mut s = docker.create_image(
        Some(CreateImageOptions {
            from_image: "alpine",
            tag: "latest",
            ..Default::default()
        }),
        None,
        None,
    );
    while let Some(_) = s.next().await {}

    let opts = CreateContainerOptions {
        name: CTR_NAME,
        ..Default::default()
    };
    let config: Config<&str> = Config {
        image: Some("alpine"),
        cmd: Some(vec!["sleep", "infinity"]),
        ..Default::default()
    };
    docker.create_container(Some(opts), config).await?;
    docker
        .start_container(CTR_NAME, None::<StartContainerOptions<&str>>)
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}

#[derive(Debug)]
enum TestAOutcome {
    Backpressured {
        bytes_written: usize,
        elapsed_ms: u128,
    },
    AllBuffered {
        bytes_written: usize,
        elapsed_ms: u128,
    },
    WriteError {
        bytes_written: usize,
        err: String,
    },
}

async fn test_backpressure(docker: &Docker) -> Result<TestAOutcome, Box<dyn std::error::Error>> {
    let exec = docker
        .create_exec(
            CTR_NAME,
            CreateExecOptions {
                cmd: Some(vec!["cat"]),
                attach_stdin: Some(true),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await?;

    let attached = docker
        .start_exec(
            &exec.id,
            Some(StartExecOptions {
                detach: false,
                ..Default::default()
            }),
        )
        .await?;

    let (mut output, mut input) = match attached {
        StartExecResults::Attached { output, input } => (output, input),
        _ => return Err("not attached".into()),
    };

    // CRITICAL: we DO NOT spawn a consumer for `output`. The whole
    // point is to see what happens when stdout isn't being read.

    let chunk = vec![0xABu8; CHUNK_BYTES];
    let started = Instant::now();
    let mut written = 0usize;

    while written < TOTAL_TO_WRITE_BYTES {
        let write_res = tokio::time::timeout(
            WRITE_TIMEOUT_PER_CHUNK,
            input.write_all(&chunk),
        )
        .await;

        match write_res {
            Ok(Ok(_)) => {
                written += CHUNK_BYTES;
                if written.is_multiple_of(8 * 1024 * 1024) {
                    eprintln!(
                        "[testA] {} MiB written so far (elapsed {}ms)",
                        written / (1024 * 1024),
                        started.elapsed().as_millis()
                    );
                }
            }
            Ok(Err(e)) => {
                return Ok(TestAOutcome::WriteError {
                    bytes_written: written,
                    err: e.to_string(),
                });
            }
            Err(_elapsed) => {
                // Timeout — write_all has been awaiting longer than
                // WRITE_TIMEOUT_PER_CHUNK. That's the signature of
                // healthy backpressure: the underlying pipe is full
                // and the producer is being held back.
                let outcome = TestAOutcome::Backpressured {
                    bytes_written: written,
                    elapsed_ms: started.elapsed().as_millis(),
                };
                // Drain a bit of output so the exec can finish quickly.
                drop(input);
                // Consume output to let exec wind down.
                let drain_started = Instant::now();
                while let Some(_) = output.next().await {
                    if drain_started.elapsed() > Duration::from_secs(2) {
                        break;
                    }
                }
                return Ok(outcome);
            }
        }
    }

    Ok(TestAOutcome::AllBuffered {
        bytes_written: written,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

#[derive(Debug)]
enum TestBOutcome {
    ProducerNoticedDrop {
        bytes_written_before_error: usize,
        err: String,
    },
    NoErrorAfterDrop {
        bytes_written: usize,
    },
}

async fn test_drop(docker: &Docker) -> Result<TestBOutcome, Box<dyn std::error::Error>> {
    let exec = docker
        .create_exec(
            CTR_NAME,
            CreateExecOptions {
                cmd: Some(vec!["cat"]),
                attach_stdin: Some(true),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await?;

    let attached = docker
        .start_exec(
            &exec.id,
            Some(StartExecOptions {
                detach: false,
                ..Default::default()
            }),
        )
        .await?;

    let (output, mut input) = match attached {
        StartExecResults::Attached { output, input } => (output, input),
        _ => return Err("not attached".into()),
    };

    // Write a small amount to confirm the channel is alive.
    let chunk = vec![0xCDu8; CHUNK_BYTES];
    input.write_all(&chunk).await?;
    eprintln!("[testB] wrote initial chunk; now dropping the output stream");

    // Drop the consumer side.
    drop(output);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Now keep writing — eventually the producer side should error.
    let mut written = CHUNK_BYTES;
    let start = Instant::now();
    loop {
        let write_res = tokio::time::timeout(
            Duration::from_secs(5),
            input.write_all(&chunk),
        )
        .await;
        match write_res {
            Ok(Ok(_)) => {
                written += CHUNK_BYTES;
                if start.elapsed() > Duration::from_secs(8) {
                    return Ok(TestBOutcome::NoErrorAfterDrop {
                        bytes_written: written,
                    });
                }
            }
            Ok(Err(e)) => {
                return Ok(TestBOutcome::ProducerNoticedDrop {
                    bytes_written_before_error: written,
                    err: e.to_string(),
                });
            }
            Err(_elapsed) => {
                // Backpressure (cat's stdout pipe filling). That's
                // expected here too, but doesn't tell us whether
                // drop-detection works. Continue.
                return Ok(TestBOutcome::NoErrorAfterDrop {
                    bytes_written: written,
                });
            }
        }
    }
}
