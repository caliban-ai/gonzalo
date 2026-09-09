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

// ── store roots expand a leading `~` even with no shell (gonzalo#238) ────────

/// `gonzalo sync` takes its two store roots as positional arguments, and those
/// were the one place #211's expansion did not reach. The failure was silent:
/// syncing `~/a` and `~/b` operated on two directories that did not exist and
/// reported `copied_to_b: 0`, which reads as "already in sync".
///
/// `Command::env` sets the variable on the **child**, so this needs no
/// `std::env::set_var` — which is `unsafe` under edition 2024 and forbidden here.
#[test]
fn sync_expands_a_leading_tilde_in_both_store_roots() {
    let home = TempDir::new().unwrap();
    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("note.md"), "hello").unwrap();

    // Build store A at $HOME/sa. `--root` already expands, so this also pins
    // that the two paths agree about where `~/sa` is.
    let seed = Command::new(bin())
        .args([
            "migrate",
            "--root",
            "~/sa",
            "--namespace",
            "ns",
            "--collection",
            "col",
        ])
        .arg(src.path())
        .env("HOME", home.path())
        .output()
        .expect("run gonzalo migrate");
    assert!(seed.status.success(), "seeding store A failed: {seed:?}");
    assert!(
        home.path().join("sa").is_dir(),
        "migrate --root '~/sa' must write under $HOME"
    );

    let out = Command::new(bin())
        .args(["sync", "~/sa", "~/sb"])
        .env("HOME", home.path())
        .output()
        .expect("run gonzalo sync");
    assert!(out.status.success(), "sync failed: {out:?}");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("copied_to_b: 1"),
        "sync must see store A's record through the tilde, got {stdout:?}"
    );
    assert!(
        home.path().join("sb").is_dir(),
        "store B must be created under $HOME, not at a literal '~'"
    );
}

/// The other half of the contract: paths without a tilde must be untouched, so
/// a relative store root stays relative to the working directory.
#[test]
fn sync_leaves_a_relative_store_root_alone() {
    let cwd = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let out = Command::new(bin())
        .args(["sync", "./ra", "./rb"])
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .output()
        .expect("run gonzalo sync");
    assert!(out.status.success(), "sync failed: {out:?}");
    assert!(
        home.path().read_dir().unwrap().next().is_none(),
        "a relative root must not land in $HOME"
    );
}
