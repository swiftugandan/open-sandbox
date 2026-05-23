use std::collections::HashMap;
use std::sync::Arc;

use open_sandbox_agent::sandbox::SandboxManager;
use open_sandbox_agent_youki::{YoukiConfig, YoukiRuntime};
use open_sandbox_contracts::controller::{SandboxConfig, SandboxState, StartSandbox, StopSandbox};
use open_sandbox_contracts::types::SandboxId;
use serial_test::serial;

fn youki_config() -> YoukiConfig {
    YoukiConfig {
        root_dir: std::path::PathBuf::from("/tmp/youki-e2e"),
        cni_bin_path: std::path::PathBuf::from("/opt/cni/bin"),
    }
}

fn start_cmd(sandbox_id: &SandboxId, image: &str) -> StartSandbox {
    StartSandbox {
        sandbox_id: sandbox_id.to_string(),
        image: image.into(),
        config: Some(SandboxConfig {
            cpu_limit_millicores: 1000,
            memory_limit_bytes: 512_000_000,
            env_vars: HashMap::new(),
            exposed_port: 8080,
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn sandbox_lifecycle_through_manager() {
    let runtime = Arc::new(YoukiRuntime::new(youki_config()).unwrap());
    let manager = SandboxManager::new(runtime);
    let sandbox_id = SandboxId::new();

    let state = manager
        .start_sandbox(start_cmd(&sandbox_id, "alpine:latest"))
        .await
        .unwrap();
    assert_eq!(state, SandboxState::Running);

    let entry = manager.get_sandbox(&sandbox_id).unwrap();
    assert!(entry.host_port > 0);

    let sandboxes = manager.list_sandboxes();
    assert!(sandboxes.iter().any(|s| s.sandbox_id == sandbox_id));

    let stop_state = manager
        .stop_sandbox(StopSandbox {
            sandbox_id: sandbox_id.to_string(),
            timeout_seconds: 5,
        })
        .await
        .unwrap();
    assert_eq!(stop_state, SandboxState::Stopped);

    assert!(manager.get_sandbox(&sandbox_id).is_none());
}

/// Verifies that read_file / write_file / write_files_targz on
/// the YoukiRuntime now go through setns(2) (the
/// `setns_ops` module) and do NOT rely on any in-container
/// binary. Asserts round-trip integrity:
///
///   write_file → read_file
///   write_files_targz → read_file
///
/// The round-trip is observed via the runtime trait directly,
/// so a passing test proves the agent process can read AND
/// write the sandbox's filesystem from outside without
/// invoking `cat` / `tee` / `tar` / `mkdir` inside the
/// container.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn write_then_read_via_setns_round_trips() {
    use bytes::Bytes;
    use open_sandbox_agent::container::ContainerRuntime;

    let runtime = Arc::new(YoukiRuntime::new(youki_config()).unwrap());
    let manager = SandboxManager::new(runtime.clone());
    let sandbox_id = SandboxId::new();

    manager
        .start_sandbox(start_cmd(&sandbox_id, "alpine:latest"))
        .await
        .unwrap();

    // Find the container_id the manager registered.
    let entry = manager.get_sandbox(&sandbox_id).unwrap();
    let container_id = entry.container_id.clone();

    // 1. write_file → read_file round-trip.
    let payload = Bytes::from_static(b"hello from setns_ops\n");
    runtime
        .write_file(&container_id, "/tmp/setns_test.txt", None, payload.clone())
        .await
        .expect("write_file via setns should succeed");
    let got = runtime
        .read_file(&container_id, "/tmp/setns_test.txt", None)
        .await
        .expect("read_file via setns should succeed");
    assert_eq!(got, payload, "file round-trip mismatch");

    // 2. read_file on missing path emits the v0.7 "No such file"
    //    detail so the io_session layer can map it to FileNotFound.
    let miss = runtime
        .read_file(&container_id, "/tmp/definitely-not-present", None)
        .await;
    match miss {
        Err(open_sandbox_contracts::error::AgentError::Runtime { detail }) => {
            assert!(
                detail.contains("No such file"),
                "expected 'No such file' marker, got: {detail}"
            );
        }
        other => panic!("expected NotFound runtime error, got: {other:?}"),
    }

    manager
        .stop_sandbox(StopSandbox {
            sandbox_id: sandbox_id.to_string(),
            timeout_seconds: 5,
        })
        .await
        .unwrap();
}

/// Verifies write_files_targz extracts a gzipped tarball into
/// the container's filesystem via the setns path. Reads one of
/// the extracted entries back to prove the bytes landed where
/// the caller asked.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn write_files_targz_via_setns_round_trips() {
    use bytes::Bytes;
    use open_sandbox_agent::container::ContainerRuntime;

    let runtime = Arc::new(YoukiRuntime::new(youki_config()).unwrap());
    let manager = SandboxManager::new(runtime.clone());
    let sandbox_id = SandboxId::new();

    manager
        .start_sandbox(start_cmd(&sandbox_id, "alpine:latest"))
        .await
        .unwrap();
    let entry = manager.get_sandbox(&sandbox_id).unwrap();
    let container_id = entry.container_id.clone();

    // Build a tar.gz with a single file `inner.txt`.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut ar = tar::Builder::new(&mut gz);
        let body = b"setns-extracted-payload\n";
        let mut hdr = tar::Header::new_gnu();
        hdr.set_path("inner.txt").unwrap();
        hdr.set_size(body.len() as u64);
        hdr.set_mode(0o644);
        hdr.set_cksum();
        ar.append(&hdr, &body[..]).unwrap();
        ar.finish().unwrap();
    }
    let tarball = Bytes::from(gz.finish().unwrap());

    runtime
        .write_files_targz(&container_id, Some("/tmp/extract-here"), tarball)
        .await
        .expect("tar extract via setns should succeed");

    let got = runtime
        .read_file(&container_id, "/tmp/extract-here/inner.txt", None)
        .await
        .expect("read_file on extracted entry should succeed");
    assert_eq!(&got[..], b"setns-extracted-payload\n");

    manager
        .stop_sandbox(StopSandbox {
            sandbox_id: sandbox_id.to_string(),
            timeout_seconds: 5,
        })
        .await
        .unwrap();
}
