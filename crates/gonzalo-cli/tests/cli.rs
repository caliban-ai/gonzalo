//! Integration tests over the `gonzalo` binary, exercising the CLI's process
//! exit contract (gonzalo#152): an absent record is a non-zero exit with an
//! empty stdout, so automation can distinguish absent from present.

use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// Path to the compiled `gonzalo` binary under test.
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gonzalo"))
}

#[test]
fn get_missing_record_exits_nonzero_with_empty_stdout() {
    let root = TempDir::new().unwrap();
    let out = Command::new(bin())
        .args(["get", "--root"])
        .arg(root.path())
        .args(["ns", "col", "does-not-exist"])
        .output()
        .expect("run gonzalo get");

    assert!(
        !out.status.success(),
        "a missing record must yield a non-zero exit, got {:?}",
        out.status
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty on the not-found path, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found"),
        "the not-found reason belongs on stderr, got {stderr:?}"
    );
}

#[test]
fn ticket_get_missing_record_exits_nonzero_with_empty_stdout() {
    let root = TempDir::new().unwrap();
    let out = Command::new(bin())
        .args(["ticket", "get", "--root"])
        .arg(root.path())
        .arg("caliban-ai/gonzalo#99999")
        .output()
        .expect("run gonzalo ticket get");

    assert!(
        !out.status.success(),
        "a missing ticket must yield a non-zero exit, got {:?}",
        out.status
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty on the not-found path, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found"),
        "the not-found reason belongs on stderr, got {stderr:?}"
    );
}

#[test]
fn get_present_record_exits_zero_and_prints_to_stdout() {
    // Migrate a file into the store, then fetch it: the happy path still exits 0
    // and emits the record JSON on stdout (guards against the #152 fix breaking
    // the present case).
    let root = TempDir::new().unwrap();
    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("note.md"), "hello").unwrap();

    let migrate = Command::new(bin())
        .args(["migrate", "--root"])
        .arg(root.path())
        .arg(src.path())
        .args(["--namespace", "ns", "--collection", "col"])
        .output()
        .expect("run gonzalo migrate");
    assert!(migrate.status.success(), "migrate should succeed");

    let out = Command::new(bin())
        .args(["get", "--root"])
        .arg(root.path())
        .args(["ns", "col", "note.md"])
        .output()
        .expect("run gonzalo get");

    assert!(out.status.success(), "present record must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("note.md"),
        "the record JSON should reach stdout, got {stdout:?}"
    );
}
