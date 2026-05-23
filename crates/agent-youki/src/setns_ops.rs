//! File operations against a container's mount namespace via
//! `setns(2)` rather than `docker exec`-style invocations of
//! `cat` / `tee` / `tar` inside the container. Removes the
//! image-side binary dependency the v1.0 refactor accidentally
//! reintroduced (closes the FOLLOWUPS_v1.0.1.md P3 item).
//!
//! ## How it works
//!
//! 1. Open `/proc/self/ns/mnt` — the agent's current mount
//!    namespace handle (so we can restore later).
//! 2. Open `/proc/<container_init_pid>/ns/mnt` — the target
//!    namespace handle.
//! 3. Call `setns(target, CLONE_NEWNS)`. The CALLING THREAD is
//!    now in the container's mount namespace and sees the
//!    container's filesystem at its real paths.
//! 4. Perform the file operation with plain `std::fs::*`.
//! 5. Restore via `setns(saved, CLONE_NEWNS)` — done in a
//!    `Drop` guard so panics restore too.
//!
//! ## Thread safety
//!
//! `setns(CLONE_NEWNS)` changes only the calling thread's
//! namespace. We MUST keep all four steps on the same OS
//! thread; we do this by running inside
//! `tokio::task::spawn_blocking`, which gives the closure
//! exclusive use of a thread for its duration. The Drop guard
//! restores the namespace before the thread returns to the
//! blocking pool — without that, the worker would leak into
//! subsequent tasks.
//!
//! ## Required capabilities
//!
//! `CAP_SYS_ADMIN` (for `setns`). The youki agent already runs
//! privileged for `libcontainer`; no new capability needed.

use std::fs::File;
use std::os::fd::AsFd;
use std::path::Path;

use bytes::Bytes;
use nix::sched::{CloneFlags, setns};

use open_sandbox_contracts::error::AgentError;

/// Run a synchronous closure inside the target process's mount
/// namespace. The closure's return type travels back out via
/// the spawned blocking task. Restores the original mount
/// namespace on any exit path (including panic).
pub async fn run_in_container_mount_ns<F, R>(target_pid: i32, f: F) -> Result<R, AgentError>
where
    F: FnOnce() -> Result<R, AgentError> + Send + 'static,
    R: Send + 'static,
{
    if target_pid <= 0 {
        return Err(AgentError::Runtime {
            detail: format!("invalid container pid {target_pid}"),
        });
    }
    tokio::task::spawn_blocking(move || {
        let save_fd = File::open("/proc/self/ns/mnt").map_err(|e| AgentError::Runtime {
            detail: format!("open self mnt ns: {e}"),
        })?;
        let target_path = format!("/proc/{target_pid}/ns/mnt");
        let target_fd = File::open(&target_path).map_err(|e| AgentError::Runtime {
            detail: format!("open target mnt ns {target_path}: {e}"),
        })?;

        setns(target_fd.as_fd(), CloneFlags::CLONE_NEWNS).map_err(|e| AgentError::Runtime {
            detail: format!("setns into target mnt ns (pid={target_pid}): {e}"),
        })?;

        // From here on, the thread is in the container's mount
        // namespace. The guard restores on drop.
        let _guard = MountNsGuard {
            save_fd: Some(save_fd),
        };
        f()
    })
    .await
    .map_err(|e| AgentError::Runtime {
        detail: format!("setns blocking task panicked: {e}"),
    })?
}

/// RAII guard that restores the original mount namespace of the
/// CALLING thread on drop. Holds the save fd; drops it after
/// the restore call.
struct MountNsGuard {
    save_fd: Option<File>,
}

impl Drop for MountNsGuard {
    fn drop(&mut self) {
        let Some(fd) = self.save_fd.take() else {
            return;
        };
        if let Err(e) = setns(fd.as_fd(), CloneFlags::CLONE_NEWNS) {
            // If restoration fails the spawn_blocking worker is
            // permanently in the container's mount namespace.
            // Tracing-only — there's no portable way to mark a
            // thread "do not reuse" in the tokio blocking pool.
            // In practice setns-back for the same caller's saved
            // fd is reliable when setns-in succeeded; this is a
            // last-resort log line.
            tracing::error!(
                error = %e,
                "FATAL: failed to restore mount namespace after setns; \
                 spawn_blocking worker is now in the wrong namespace"
            );
        }
    }
}

/// Read a file inside the container's mount namespace. Returns
/// `Runtime { detail: "No such file: ..." }` when the path is
/// missing (preserves the `FileNotFound` resolved-path promise
/// the io_session layer turns into the API's structured error).
pub async fn read_file_in_ns(target_pid: i32, path: String) -> Result<Bytes, AgentError> {
    run_in_container_mount_ns(target_pid, move || match std::fs::read(&path) {
        Ok(bytes) => Ok(Bytes::from(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AgentError::Runtime {
            detail: format!("No such file: {path}"),
        }),
        Err(e) => Err(AgentError::Runtime {
            detail: format!("read {path}: {e}"),
        }),
    })
    .await
}

/// Write a file atomically (temp + rename) inside the
/// container's mount namespace. Auto-creates missing parent
/// directories. The temp file lives next to the target so the
/// rename is within a single filesystem.
pub async fn write_file_in_ns(
    target_pid: i32,
    path: String,
    content: Bytes,
) -> Result<(), AgentError> {
    let parent = Path::new(&path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".into());
    let temp = format!("{parent}/.opensb.{}.tmp", uuid::Uuid::new_v4().simple());
    let path_for_move = path.clone();
    run_in_container_mount_ns(target_pid, move || {
        if !parent.is_empty() {
            std::fs::create_dir_all(&parent).map_err(|e| AgentError::Runtime {
                detail: format!("mkdir -p {parent}: {e}"),
            })?;
        }
        std::fs::write(&temp, &content[..]).map_err(|e| AgentError::Runtime {
            detail: format!("write temp {temp}: {e}"),
        })?;
        std::fs::rename(&temp, &path_for_move).map_err(|e| AgentError::Runtime {
            detail: format!("rename {temp} -> {path_for_move}: {e}"),
        })?;
        Ok(())
    })
    .await
}

/// Extract a gzipped tarball into a target directory inside the
/// container's mount namespace. Creates the target directory if
/// needed.
pub async fn extract_targz_in_ns(
    target_pid: i32,
    target_dir: String,
    tarball: Bytes,
) -> Result<(), AgentError> {
    run_in_container_mount_ns(target_pid, move || {
        std::fs::create_dir_all(&target_dir).map_err(|e| AgentError::Runtime {
            detail: format!("mkdir -p {target_dir}: {e}"),
        })?;
        let gz = flate2::read::GzDecoder::new(&tarball[..]);
        let mut archive = tar::Archive::new(gz);
        archive
            .unpack(&target_dir)
            .map_err(|e| AgentError::Runtime {
                detail: format!("tar extract into {target_dir}: {e}"),
            })?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_pid_returns_runtime_error_not_panic() {
        // PID 0 / negative cannot be a real container init. The
        // setns layer should reject early with a Runtime error
        // rather than reaching the proc path open (which would
        // produce a less actionable error).
        let result = read_file_in_ns(0, "/anything".into()).await;
        match result {
            Err(AgentError::Runtime { detail }) => {
                assert!(
                    detail.contains("invalid container pid"),
                    "expected early pid validation, got: {detail}"
                );
            }
            other => panic!("expected Runtime error, got: {other:?}"),
        }
    }

    /// Smoke test that doesn't require any container — verifies
    /// that opening /proc/self/ns/mnt + setns'ing back into
    /// ourselves is a no-op round-trip. Skipped on hosts that
    /// disallow ns operations (e.g. running as a non-root
    /// non-CAP_SYS_ADMIN test runner).
    #[tokio::test]
    async fn setns_self_round_trip_is_noop() {
        // Skip if we can't even read our own mnt ns (containers
        // without /proc, etc).
        if std::fs::File::open("/proc/self/ns/mnt").is_err() {
            eprintln!("/proc/self/ns/mnt not readable — skipping");
            return;
        }
        let my_pid = std::process::id() as i32;
        let result = run_in_container_mount_ns(my_pid, || {
            // We're in our own mount ns — should still see this
            // path the test runner can stat.
            if !Path::new("/").exists() {
                return Err(AgentError::Runtime {
                    detail: "/ disappeared".into(),
                });
            }
            Ok(42u32)
        })
        .await;
        match result {
            Ok(42) => {}
            Err(AgentError::Runtime { detail }) if detail.contains("setns into target mnt ns") => {
                // Permitted: kernel rejects setns into our own
                // ns without sufficient privilege. Acceptable on
                // CI runners that aren't privileged.
                eprintln!("setns rejected by kernel (expected on non-priv test host): {detail}");
            }
            other => panic!("unexpected result from self-ns round trip: {other:?}"),
        }
    }
}
