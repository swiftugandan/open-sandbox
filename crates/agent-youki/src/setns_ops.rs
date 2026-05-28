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
//! namespace. The kernel additionally requires that the
//! calling thread's `fs_struct` (cwd/root/umask) is NOT shared
//! with any other thread — `mntns_install` returns `EINVAL`
//! if `fs->users != 1`. Linux threads in the same process
//! share `fs_struct` by default, so we must call
//! `unshare(CLONE_FS)` before `setns(CLONE_NEWNS)`. That call
//! is irreversible (the kernel won't let you re-share), which
//! means we cannot safely reuse a tokio blocking-pool worker
//! after running setns on it.
//!
//! Therefore each call runs on a fresh, short-lived OS thread
//! via `std::thread::spawn` — the thread terminates as soon as
//! the closure returns, taking its private `fs_struct` with
//! it. The async layer bridges the join via a tokio oneshot.
//!
//! ## Required capabilities
//!
//! `CAP_SYS_ADMIN` (for `setns` + `unshare`). The youki agent
//! already runs privileged for `libcontainer`; no new
//! capability needed.

use std::fs::File;
use std::os::fd::AsFd;
use std::path::Path;

use bytes::Bytes;
use nix::sched::{CloneFlags, setns, unshare};

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
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<R, AgentError>>();
    std::thread::Builder::new()
        .name(format!("setns-{target_pid}"))
        .spawn(move || {
            let result = setns_op_inner(target_pid, f);
            let _ = tx.send(result);
        })
        .map_err(|e| AgentError::Runtime {
            detail: format!("spawn setns thread: {e}"),
        })?;
    rx.await.map_err(|e| AgentError::Runtime {
        detail: format!("setns thread dropped sender: {e}"),
    })?
}

fn setns_op_inner<F, R>(target_pid: i32, f: F) -> Result<R, AgentError>
where
    F: FnOnce() -> Result<R, AgentError>,
{
    let save_fd = File::open("/proc/self/ns/mnt").map_err(|e| AgentError::Runtime {
        detail: format!("open self mnt ns: {e}"),
    })?;
    let target_path = format!("/proc/{target_pid}/ns/mnt");
    let target_fd = File::open(&target_path).map_err(|e| AgentError::Runtime {
        detail: format!("open target mnt ns {target_path}: {e}"),
    })?;

    // Detach this thread's fs_struct from the rest of the
    // process. Linux requires fs->users == 1 for setns(MNT);
    // without this we get EINVAL from mntns_install. Safe
    // because the thread terminates after this op completes.
    unshare(CloneFlags::CLONE_FS).map_err(|e| AgentError::Runtime {
        detail: format!("unshare CLONE_FS before setns: {e}"),
    })?;

    setns(target_fd.as_fd(), CloneFlags::CLONE_NEWNS).map_err(|e| AgentError::Runtime {
        detail: format!("setns into target mnt ns (pid={target_pid}): {e}"),
    })?;

    // Guard restores our original mount namespace on drop —
    // important so that any errno-extracting code (e.g.,
    // strerror) outside the closure runs in our home ns rather
    // than the container's. The thread will then terminate.
    let _guard = MountNsGuard {
        save_fd: Some(save_fd),
    };
    f()
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

/// Sandbox-internal allowlist for `extract_targz_in_ns`. Comp-5: client
/// can otherwise write tarball entries to `/etc`, `/usr/bin`, etc. inside
/// the container, planting setuid binaries or overriding the shell. The
/// allowed prefixes here are the only `cwd` values write_files_targz
/// will accept.
///
/// These are intentionally writable areas in a standard sandbox layout.
/// If your image expects writes elsewhere, run a small shim that
/// rewrites the target before calling.
pub const WRITE_TARGZ_ALLOWED_PREFIXES: &[&str] =
    &["/workspace", "/home", "/tmp", "/var/tmp"];

/// Extract a gzipped tarball into a target directory inside the
/// container's mount namespace. Creates the target directory if
/// needed.
///
/// Comp-5: rejects target_dir outside [`WRITE_TARGZ_ALLOWED_PREFIXES`].
/// Prevents a client from planting binaries in `/etc` or `/usr/bin`
/// by passing those as `cwd`.
pub async fn extract_targz_in_ns(
    target_pid: i32,
    target_dir: String,
    tarball: Bytes,
) -> Result<(), AgentError> {
    if !is_target_dir_allowed(&target_dir) {
        return Err(AgentError::Runtime {
            detail: format!(
                "target_dir {target_dir:?} is not under an allowed prefix; \
                 allowed: {WRITE_TARGZ_ALLOWED_PREFIXES:?}"
            ),
        });
    }
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

fn is_target_dir_allowed(path: &str) -> bool {
    // Reject any `..` so a path under an allowed prefix can't escape.
    if path.split('/').any(|seg| seg == "..") {
        return false;
    }
    WRITE_TARGZ_ALLOWED_PREFIXES
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

/// v1.0.3: list one level of a directory inside the container's
/// mount namespace. Operates via std::fs after setns(2) — same
/// pattern as read_file_in_ns. Hard-caps at LIST_DIR_MAX_ENTRIES
/// per the v1.0.3 contract.
///
/// Returns a vector of `(DirEntry, /* sentinel: was_truncated */ bool)`
/// where the bool slot is `true` only on the last returned entry
/// when the underlying dir had more entries than the cap. Total
/// count is the second return value. The agent crate wraps these
/// into the trait's `DirListing`.
pub async fn list_dir_in_ns(
    target_pid: i32,
    path: String,
) -> Result<NsDirListing, AgentError> {
    use open_sandbox_contracts::constants::LIST_DIR_MAX_ENTRIES;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    run_in_container_mount_ns(target_pid, move || {
        let read_dir = std::fs::read_dir(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AgentError::Runtime {
                detail: format!("No such file: {path}"),
            },
            _ => AgentError::Runtime {
                detail: format!("read_dir {path}: {e}"),
            },
        })?;

        let mut entries: Vec<NsDirEntry> = Vec::new();
        let mut total_entries: u64 = 0;
        for raw in read_dir {
            total_entries += 1;
            let entry = match raw {
                Ok(e) => e,
                Err(_) => continue, // skip entries we can't read
            };
            if entries.len() >= LIST_DIR_MAX_ENTRIES {
                // Continue counting to populate total_entries, but
                // don't grow `entries` past the cap.
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Use symlink_metadata so symlinks don't follow.
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let file_type = meta.file_type();
            let (entry_type, target) = if file_type.is_symlink() {
                let target = std::fs::read_link(entry.path())
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (NsEntryType::Symlink, target)
            } else if file_type.is_dir() {
                (NsEntryType::Dir, String::new())
            } else if file_type.is_file() {
                (NsEntryType::File, String::new())
            } else {
                (NsEntryType::Other, String::new())
            };

            let size = if entry_type == NsEntryType::Dir {
                0
            } else {
                meta.len()
            };
            let mtime = meta.mtime() as u64;
            let revision = format!("{mtime}:{size}");
            let mode = format!("{:04o}", meta.permissions().mode() & 0o7777);

            entries.push(NsDirEntry {
                name,
                entry_type,
                size,
                revision,
                mode,
                target,
            });
        }
        // Sort deterministically by name so the API returns stable
        // ordering across calls (readdir order is filesystem-
        // dependent).
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        let truncated = total_entries as usize > entries.len();
        Ok(NsDirListing {
            path,
            entries,
            truncated,
            total_entries,
        })
    })
    .await
}

/// v1.0.3: stat a single path inside the container's mount
/// namespace and return the agent's opaque revision token.
/// Reference encoding: `<mtime_secs>:<size>`. Matches the agent-
/// docker shell-exec'd `stat` output 1:1 so a sandbox migrated
/// between runtimes mid-session retains revision continuity.
pub async fn stat_revision_in_ns(
    target_pid: i32,
    path: String,
) -> Result<NsFileRevision, AgentError> {
    use std::os::unix::fs::MetadataExt;

    run_in_container_mount_ns(target_pid, move || {
        let meta = std::fs::symlink_metadata(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AgentError::Runtime {
                detail: format!("No such file: {path}"),
            },
            _ => AgentError::Runtime {
                detail: format!("stat {path}: {e}"),
            },
        })?;
        let mtime = meta.mtime() as u64;
        let size = if meta.is_dir() { 0 } else { meta.len() };
        Ok(NsFileRevision {
            revision: format!("{mtime}:{size}"),
            size,
        })
    })
    .await
}

/// Cross-crate transport for the ns-bound list_dir result. The
/// agent-crate-side wrapper translates these into the
/// `open_sandbox_agent::container::DirListing` shape; we don't
/// import that type here to keep setns_ops independent of the
/// agent crate's domain types.
#[derive(Debug, Clone)]
pub struct NsDirListing {
    pub path: String,
    pub entries: Vec<NsDirEntry>,
    pub truncated: bool,
    pub total_entries: u64,
}

#[derive(Debug, Clone)]
pub struct NsDirEntry {
    pub name: String,
    pub entry_type: NsEntryType,
    pub size: u64,
    pub revision: String,
    pub mode: String,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsEntryType {
    File,
    Dir,
    Symlink,
    Other,
}

/// Cross-crate transport for stat_revision_in_ns. Same rationale
/// as NsDirListing: keeps setns_ops free of agent-crate-side
/// imports.
#[derive(Debug, Clone)]
pub struct NsFileRevision {
    pub revision: String,
    pub size: u64,
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
