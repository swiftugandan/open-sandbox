//! Benchmark: measure `YoukiRuntime::create_and_start` wall-clock on Linux.
//!
//! Times N create_and_start calls for each `PullPolicy` (IfNotPresent +
//! Always) against `alpine:latest`, cleaning up between each iteration.
//! Pre-warms the digest cache once so the first sample of IfNotPresent
//! is a warm hit, not a cold pull.
//!
//! Run inside the agent-youki dev container (Linux required because
//! libcontainer + cgroup v2 + cni; macOS host cannot build/link the
//! agent-youki crate at all):
//!
//! ```sh
//! docker compose -f crates/agent-youki/docker-compose.dev.yml up -d
//! docker compose -f crates/agent-youki/docker-compose.dev.yml exec dev \
//!     cargo run --release --example bench_create_and_start \
//!         -p open-sandbox-agent-youki
//! ```
//!
//! Output is per-iteration timings plus a min/p50/p90/max/mean summary
//! line per policy. Compare against the docker-runtime measurement
//! harness at `/tmp/exp-logs/measure.py` (host-side, against the dev
//! fleet's HTTP API) to contrast the two runtimes on the same machine.
//!
//! Caveat: when run inside Docker Desktop's Linux VM on macOS, the
//! syscall + io paths cost more than they would on bare-metal Linux
//! (~50–100ms on the youki create+start path). Manifest-fetch RTT
//! to Docker Hub is the dominant cost on both runtimes regardless.

use std::sync::Arc;
use std::time::{Duration, Instant};

use open_sandbox_agent::container::{ContainerConfig, ContainerRuntime};
use open_sandbox_agent_youki::{YoukiConfig, YoukiRuntime};
use open_sandbox_contracts::types::{PullPolicy, SandboxId};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = YoukiConfig {
        root_dir: tmp.path().to_path_buf(),
        cni_bin_path: std::path::PathBuf::from("/opt/cni/bin"),
    };
    let runtime = Arc::new(YoukiRuntime::new(cfg).expect("YoukiRuntime::new"));

    let warm = sandbox_config(SandboxId::new(), PullPolicy::IfNotPresent);
    eprintln!("pre-warm: pulling alpine:latest…");
    let prewarm_start = Instant::now();
    let info = runtime
        .create_and_start(warm)
        .await
        .expect("pre-warm create_and_start");
    eprintln!(
        "pre-warm done in {:.0}ms",
        prewarm_start.elapsed().as_secs_f64() * 1000.0
    );
    runtime
        .stop_and_remove(&info.id, Duration::from_secs(5))
        .await
        .ok();

    for policy in [PullPolicy::IfNotPresent, PullPolicy::Always] {
        let mut samples_ms: Vec<f64> = Vec::new();
        let n = 10usize;
        for i in 0..n {
            let cfg = sandbox_config(SandboxId::new(), policy);
            let t0 = Instant::now();
            let info = runtime
                .create_and_start(cfg)
                .await
                .expect("create_and_start");
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            samples_ms.push(elapsed_ms);
            eprintln!("  {:?} run {i}: {elapsed_ms:.1}ms", policy);
            runtime
                .stop_and_remove(&info.id, Duration::from_secs(5))
                .await
                .ok();
        }
        samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let min = samples_ms[0];
        let p50 = samples_ms[n / 2];
        let p90 = samples_ms[(n * 9) / 10];
        let max = samples_ms[n - 1];
        let mean = samples_ms.iter().sum::<f64>() / n as f64;
        println!(
            "{:?}: n={n} min={min:.0} p50={p50:.0} p90={p90:.0} max={max:.0} mean={mean:.0}",
            policy
        );
    }
}

fn sandbox_config(sandbox_id: SandboxId, policy: PullPolicy) -> ContainerConfig {
    ContainerConfig {
        sandbox_id,
        image: "alpine:latest".to_string(),
        cpu_limit_millicores: 1000,
        memory_limit_bytes: 512 * 1024 * 1024,
        env_vars: std::collections::HashMap::new(),
        exposed_port: 8080,
        pull_policy: policy,
    }
}
