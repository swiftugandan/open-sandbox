//! contracts/v1.0.3 wire-shape regression tests.
//!
//! Each test pins one of the v1.0.3 additions:
//!
//!  * `ListDirParams` / `ListDirEntry` / `ListDirResult` — typed
//!    one-level directory listing, used by the UI file tree.
//!  * `WaitPortListeningParams` / `WaitPortListeningResult` — single-
//!    shot TCP-probe of the sandbox's host_port, used by the UI's
//!    save-chain to know when the in-container dev-server has come
//!    back up before refreshing the preview iframe.
//!  * `FileMeta` (server frame) + revision fields on `WriteFileParams`
//!    — opaque revision token + `expected_revision` enforcement that
//!    closes the external-mutation reconciliation gap.
//!
//! These live as a separate `tests/` file (not inline `#[cfg(test)]`)
//! so the wire shape is exercised through the public crate surface,
//! not through `pub(crate)` internals.

use open_sandbox_contracts::proxy;
use prost::Message;

// ─── ListDirParams + ListDirResult ──────────────────────────────────

#[test]
fn list_dir_params_roundtrip() {
    let original = proxy::ListDirParams {
        path: "/workspace".to_string(),
        cwd: "/workspace".to_string(),
    };
    let bytes = original.encode_to_vec();
    let decoded = proxy::ListDirParams::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.path, "/workspace");
    assert_eq!(decoded.cwd, "/workspace");
}

#[test]
fn list_dir_entry_roundtrips_all_fields() {
    let original = proxy::ListDirEntry {
        name: "src".to_string(),
        // prost emits the field as `r#type` (raw identifier) because
        // `type` is a Rust keyword. The wire name is `type`.
        r#type: proxy::ListDirEntryType::Dir as i32,
        size: 0,
        revision: "1716800123:0".to_string(),
        mode: "0755".to_string(),
        target: String::new(),
    };
    let bytes = original.encode_to_vec();
    let decoded = proxy::ListDirEntry::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.name, "src");
    assert_eq!(decoded.r#type, proxy::ListDirEntryType::Dir as i32);
    assert_eq!(decoded.revision, "1716800123:0");
    assert_eq!(decoded.mode, "0755");
}

#[test]
fn list_dir_entry_symlink_carries_target() {
    let original = proxy::ListDirEntry {
        name: "logs".to_string(),
        r#type: proxy::ListDirEntryType::Symlink as i32,
        size: 16,
        revision: "1716800100:16".to_string(),
        mode: "0777".to_string(),
        target: "/var/log".to_string(),
    };
    let bytes = original.encode_to_vec();
    let decoded = proxy::ListDirEntry::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.r#type, proxy::ListDirEntryType::Symlink as i32);
    assert_eq!(decoded.target, "/var/log");
}

#[test]
fn list_dir_result_truncated_flag_roundtrips() {
    let original = proxy::ListDirResult {
        path: "/workspace".to_string(),
        entries: vec![],
        truncated: true,
        total_entries: 7321,
    };
    let bytes = original.encode_to_vec();
    let decoded = proxy::ListDirResult::decode(bytes.as_slice()).unwrap();
    assert!(decoded.truncated);
    assert_eq!(decoded.total_entries, 7321);
}

#[test]
fn list_dir_params_is_an_io_start_variant() {
    use proxy::io_start::Params;
    let start = proxy::IoStart {
        sandbox_id: "abc".to_string(),
        params: Some(Params::ListDir(proxy::ListDirParams {
            path: "/workspace".to_string(),
            cwd: String::new(),
        })),
    };
    let bytes = start.encode_to_vec();
    let decoded = proxy::IoStart::decode(bytes.as_slice()).unwrap();
    match decoded.params {
        Some(Params::ListDir(p)) => assert_eq!(p.path, "/workspace"),
        other => panic!("expected ListDir variant, got {other:?}"),
    }
}

#[test]
fn list_dir_result_is_an_io_server_frame_payload() {
    use proxy::io_server_frame::Payload;
    let frame = proxy::IoServerFrame {
        stream_id: "s1".to_string(),
        payload: Some(Payload::ListDirResult(proxy::ListDirResult {
            path: "/workspace".to_string(),
            entries: vec![],
            truncated: false,
            total_entries: 0,
        })),
    };
    let bytes = frame.encode_to_vec();
    let decoded = proxy::IoServerFrame::decode(bytes.as_slice()).unwrap();
    match decoded.payload {
        Some(Payload::ListDirResult(r)) => assert_eq!(r.path, "/workspace"),
        other => panic!("expected ListDirResult payload, got {other:?}"),
    }
}

// ─── WaitPortListeningParams + WaitPortListeningResult ──────────────

#[test]
fn wait_port_listening_params_roundtrip() {
    let original = proxy::WaitPortListeningParams {
        port: 8080,
        timeout_ms: 3000,
    };
    let bytes = original.encode_to_vec();
    let decoded = proxy::WaitPortListeningParams::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.port, 8080);
    assert_eq!(decoded.timeout_ms, 3000);
}

#[test]
fn wait_port_listening_params_is_an_io_start_variant() {
    use proxy::io_start::Params;
    let start = proxy::IoStart {
        sandbox_id: "abc".to_string(),
        params: Some(Params::WaitPortListening(proxy::WaitPortListeningParams {
            port: 3000,
            timeout_ms: 5_000,
        })),
    };
    let bytes = start.encode_to_vec();
    let decoded = proxy::IoStart::decode(bytes.as_slice()).unwrap();
    match decoded.params {
        Some(Params::WaitPortListening(p)) => {
            assert_eq!(p.port, 3000);
            assert_eq!(p.timeout_ms, 5_000);
        }
        other => panic!("expected WaitPortListening variant, got {other:?}"),
    }
}

#[test]
fn wait_port_listening_result_roundtrip() {
    let ready = proxy::WaitPortListeningResult {
        ready: true,
        elapsed_ms: 412,
    };
    let bytes = ready.encode_to_vec();
    let decoded = proxy::WaitPortListeningResult::decode(bytes.as_slice()).unwrap();
    assert!(decoded.ready);
    assert_eq!(decoded.elapsed_ms, 412);

    let not_ready = proxy::WaitPortListeningResult {
        ready: false,
        elapsed_ms: 3000,
    };
    let bytes = not_ready.encode_to_vec();
    let decoded = proxy::WaitPortListeningResult::decode(bytes.as_slice()).unwrap();
    assert!(!decoded.ready);
    assert_eq!(decoded.elapsed_ms, 3000);
}

#[test]
fn wait_port_listening_result_is_an_io_server_frame_payload() {
    use proxy::io_server_frame::Payload;
    let frame = proxy::IoServerFrame {
        stream_id: "s1".to_string(),
        payload: Some(Payload::WaitPortListeningResult(
            proxy::WaitPortListeningResult {
                ready: true,
                elapsed_ms: 50,
            },
        )),
    };
    let bytes = frame.encode_to_vec();
    let decoded = proxy::IoServerFrame::decode(bytes.as_slice()).unwrap();
    match decoded.payload {
        Some(Payload::WaitPortListeningResult(r)) => assert!(r.ready),
        other => panic!(
            "expected WaitPortListeningResult payload, got {other:?}"
        ),
    }
}

// ─── FileMeta (server frame) + revision on WriteFileParams ──────────

#[test]
fn write_file_params_carries_expected_revision_and_force() {
    let p = proxy::WriteFileParams {
        path: "/workspace/app.py".to_string(),
        cwd: String::new(),
        expected_revision: "1716800123:421".to_string(),
        force: false,
    };
    let bytes = p.encode_to_vec();
    let decoded = proxy::WriteFileParams::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.expected_revision, "1716800123:421");
    assert!(!decoded.force);
}

#[test]
fn write_file_params_default_revision_is_empty_string() {
    // Wire-compat: a v1.0.1 sender that doesn't set expected_revision
    // will encode the field as default (empty string). The gateway in
    // group C treats empty as "no precondition" — it does NOT enforce
    // revision. This test pins that default.
    let p = proxy::WriteFileParams {
        path: "/x".to_string(),
        cwd: String::new(),
        expected_revision: String::new(),
        force: false,
    };
    let bytes = p.encode_to_vec();
    let decoded = proxy::WriteFileParams::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.expected_revision, "");
}

#[test]
fn file_meta_carries_revision_and_size() {
    let meta = proxy::FileMeta {
        revision: "1716800123:421".to_string(),
        size: 421,
    };
    let bytes = meta.encode_to_vec();
    let decoded = proxy::FileMeta::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.revision, "1716800123:421");
    assert_eq!(decoded.size, 421);
}

#[test]
fn file_meta_is_an_io_server_frame_payload() {
    use proxy::io_server_frame::Payload;
    let frame = proxy::IoServerFrame {
        stream_id: "s1".to_string(),
        payload: Some(Payload::FileMeta(proxy::FileMeta {
            revision: "1716800123:421".to_string(),
            size: 421,
        })),
    };
    let bytes = frame.encode_to_vec();
    let decoded = proxy::IoServerFrame::decode(bytes.as_slice()).unwrap();
    match decoded.payload {
        Some(Payload::FileMeta(m)) => assert_eq!(m.size, 421),
        other => panic!("expected FileMeta payload, got {other:?}"),
    }
}
