use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_SOURCE_TOKENS: &[&str] = &[
    "openpty",
    "PtyProxy",
    "PtyPair",
    "setup_child_pty",
    "TIOCSCTTY",
    "wait_for_child_with_pty",
    "proxy_master_to_client",
    "proxy_client_to_master",
    "handle_pty_poll_events",
    "handle_pty_detach_request",
    "attach_listener",
    "bind_attach_listener",
    "authenticate_attach_peer",
    "decode_attach_handshake",
    "remove_stale_attach_socket",
    "take_detach_request",
    "write_detach_notice",
];

fn rust_sources(root: &Path, paths: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root.display()));
    for entry in entries {
        let path = entry.expect("failed to read source entry").path();
        if path.is_dir() {
            rust_sources(&path, paths);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.push(path);
        }
    }
}

#[test]
fn production_sources_do_not_restore_pty_runtime() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    rust_sources(&manifest_dir.join("src"), &mut paths);

    let mut violations = Vec::new();
    for path in paths {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        for token in FORBIDDEN_SOURCE_TOKENS {
            if source.contains(token) {
                violations.push(format!("{} contains {token}", path.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "removed PTY runtime symbols returned:\n{}",
        violations.join("\n")
    );
}

#[test]
fn cli_manifest_does_not_restore_vt100() {
    let manifest = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("failed to read nono-cli Cargo.toml");

    assert!(
        !manifest.contains("vt100"),
        "nono-cli must not restore the terminal screen parser dependency"
    );
}
