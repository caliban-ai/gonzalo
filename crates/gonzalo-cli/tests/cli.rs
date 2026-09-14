//! Integration tests over the `gonzalo` binary, exercising the CLI's process
//! exit contract (gonzalo#152): an absent record is a non-zero exit with an
//! empty stdout, so automation can distinguish absent from present.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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
/// reported `copied_to_b:    0`, which reads as "already in sync".
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
        stdout.contains("copied_to_b:    1"),
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

// ── delete / reset / collect over tombstones (gonzalo#203, spec §3.10) ──────

/// Run `gonzalo <args> --root <root>`.
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .arg("--root")
        .arg(root)
        .output()
        .expect("run gonzalo")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `gonzalo list` output as sorted lines (the fs store lists in OS directory
/// order, which is not stable across platforms).
fn listed_keys(root: &Path) -> Vec<String> {
    let out = run(root, &["list"]);
    assert!(out.status.success(), "{out:?}");
    let mut keys: Vec<String> = stdout(&out).lines().map(str::to_owned).collect();
    keys.sort();
    keys
}

/// Import one small file per id into `namespace/collection` (id = file name).
fn seed(root: &Path, namespace: &str, collection: &str, ids: &[&str]) {
    let src = TempDir::new().unwrap();
    for id in ids {
        std::fs::write(src.path().join(id), format!("body of {id}")).unwrap();
    }
    let out = Command::new(bin())
        .args([
            "migrate",
            "--namespace",
            namespace,
            "--collection",
            collection,
            "--root",
        ])
        .arg(root)
        .arg(src.path())
        .output()
        .expect("run gonzalo migrate");
    assert!(out.status.success(), "seeding failed: {out:?}");
}

/// The on-disk JSON file of a record in the fs store.
fn record_file(root: &Path, namespace: &str, collection: &str, id: &str) -> PathBuf {
    let key = gonzalo_core::RecordKey::new(namespace, collection, id);
    let (ns, col, file) = gonzalo_core::record_components(&key);
    root.join(ns).join(col).join(file)
}

/// Rewrite a stored record's JSON in place.
fn edit_record_file(
    path: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) {
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    edit(value.as_object_mut().expect("record JSON is an object"));
    std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

#[test]
fn delete_hides_the_record_and_exits_zero() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);

    let out = run(
        root.path(),
        &[
            "delete",
            "--namespace",
            "ns",
            "--collection",
            "col",
            "--id",
            "note.md",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(stdout(&out), "deleted: ns/col/note.md\n");

    let get = run(root.path(), &["get", "ns", "col", "note.md"]);
    assert!(!get.status.success(), "a deleted record reads as absent");
    assert!(
        record_file(root.path(), "ns", "col", "note.md").is_file(),
        "the tombstone stays on disk"
    );
}

#[test]
fn delete_of_an_absent_record_exits_zero() {
    let root = TempDir::new().unwrap();
    let out = run(
        root.path(),
        &[
            "delete",
            "--namespace",
            "ns",
            "--collection",
            "col",
            "--id",
            "nope",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(stdout(&out), "deleted: ns/col/nope\n");
}

#[test]
fn delete_with_a_stale_expected_revision_exits_three_and_keeps_the_record() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);

    let out = run(
        root.path(),
        &[
            "delete",
            "--namespace",
            "ns",
            "--collection",
            "col",
            "--id",
            "note.md",
            "--expected",
            r#"{"counter":99,"hash":"deadbeef"}"#,
        ],
    );
    assert_eq!(out.status.code(), Some(3), "a conflict exits 3: {out:?}");
    let text = stdout(&out);
    assert!(text.starts_with("conflict: ns/col/note.md\n"), "{text:?}");
    assert!(text.contains("current:  {\"counter\":0,"), "{text:?}");

    let get = run(root.path(), &["get", "ns", "col", "note.md"]);
    assert!(get.status.success(), "a conflicted delete writes nothing");
}

#[test]
fn delete_with_the_revision_get_printed_succeeds() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);
    let get = run(root.path(), &["get", "ns", "col", "note.md"]);
    let record: serde_json::Value = serde_json::from_slice(&get.stdout).unwrap();
    let revision = record["revision"].to_string();

    let out = run(
        root.path(),
        &[
            "delete",
            "--namespace",
            "ns",
            "--collection",
            "col",
            "--id",
            "note.md",
            "--expected",
            &revision,
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn delete_rejects_a_malformed_expected_revision_at_parse_time() {
    let root = TempDir::new().unwrap();
    let out = run(
        root.path(),
        &[
            "delete",
            "--namespace",
            "ns",
            "--collection",
            "col",
            "--id",
            "x",
            "--expected",
            "3",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        stderr(&out).contains("expected a revision as JSON"),
        "{out:?}"
    );
}

#[test]
fn help_documents_each_commands_exit_codes() {
    for (command, line) in [
        (
            "delete",
            "Exit codes: 0 deleted, 1 error, 2 usage error, 3 conflict",
        ),
        (
            "reset",
            "Exit codes: 0 no conflicts, 1 error, 2 usage error, 3 one or more conflicts",
        ),
        (
            "collect",
            "Exit codes: 0 success (conflicts are reported, not failures), 1 error, 2 usage error",
        ),
    ] {
        let out = Command::new(bin())
            .args([command, "--help"])
            .output()
            .expect("run gonzalo <command> --help");
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        assert!(
            stdout(&out).contains(line),
            "{command} --help must state {line:?}, got {:?}",
            stdout(&out)
        );
    }
}

#[test]
fn a_zero_ancestor_cap_is_an_error_and_touches_nothing() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);
    let out = run(
        root.path(),
        &[
            "delete",
            "--namespace",
            "ns",
            "--collection",
            "col",
            "--id",
            "note.md",
            "--ancestor-cap",
            "0",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).contains("ancestor cap must be at least 1"),
        "{out:?}"
    );
    assert!(
        run(root.path(), &["get", "ns", "col", "note.md"])
            .status
            .success()
    );
}

#[test]
fn reset_prints_its_summary_scopes_to_the_prefix_and_is_idempotent() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["a.md", "b.md"]);
    seed(root.path(), "ns", "other", &["c.md"]);
    seed(root.path(), "keep", "col", &["d.md"]);

    let out = run(
        root.path(),
        &["reset", "--namespace", "ns", "--collection", "col"],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(stdout(&out), "2 deleted, 0 conflicts\n");

    // `FsStore::list` walks directories in OS order, so compare sorted lines.
    assert_eq!(listed_keys(root.path()), ["keep/col/d.md", "ns/other/c.md"]);

    let again = run(
        root.path(),
        &["reset", "--namespace", "ns", "--collection", "col"],
    );
    assert_eq!(again.status.code(), Some(0), "{again:?}");
    assert_eq!(stdout(&again), "0 deleted, 0 conflicts\n");

    let whole = run(root.path(), &["reset", "--namespace", "ns"]);
    assert_eq!(stdout(&whole), "1 deleted, 0 conflicts\n");
    assert_eq!(listed_keys(root.path()), ["keep/col/d.md"]);
}

#[test]
fn reset_without_namespace_is_a_usage_error() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["a.md"]);
    let out = run(root.path(), &["reset", "--collection", "col"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(stderr(&out).contains("--namespace"), "{out:?}");
    assert!(
        run(root.path(), &["get", "ns", "col", "a.md"])
            .status
            .success()
    );
}

#[test]
fn collect_without_older_than_is_a_usage_error() {
    let root = TempDir::new().unwrap();
    let out = run(root.path(), &["collect", "--namespace", "ns"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(stderr(&out).contains("--older-than"), "{out:?}");
}

#[test]
fn collect_rejects_a_bad_horizon_and_collection_without_namespace() {
    let root = TempDir::new().unwrap();
    let bad = run(root.path(), &["collect", "--older-than", "30x"]);
    assert_eq!(bad.status.code(), Some(2), "{bad:?}");
    assert!(stderr(&bad).contains("unknown unit"), "{bad:?}");

    let orphan = run(
        root.path(),
        &["collect", "--older-than", "1d", "--collection", "col"],
    );
    assert_eq!(orphan.status.code(), Some(2), "{orphan:?}");
    assert!(stderr(&orphan).contains("--namespace"), "{orphan:?}");
}

#[test]
fn collect_purges_old_tombstones_and_reports_what_it_kept() {
    let root = TempDir::new().unwrap();
    seed(
        root.path(),
        "ns",
        "col",
        &["old.md", "young.md", "unstamped.md", "live.md"],
    );
    for id in ["old.md", "young.md", "unstamped.md"] {
        let out = run(
            root.path(),
            &[
                "delete",
                "--namespace",
                "ns",
                "--collection",
                "col",
                "--id",
                id,
            ],
        );
        assert!(out.status.success(), "{out:?}");
    }
    // Age one tombstone to the epoch and strip the stamp from another.
    edit_record_file(&record_file(root.path(), "ns", "col", "old.md"), |r| {
        r.insert("deleted_at".into(), serde_json::json!(0));
    });
    edit_record_file(
        &record_file(root.path(), "ns", "col", "unstamped.md"),
        |r| {
            r.remove("deleted_at");
        },
    );

    let out = run(root.path(), &["collect", "--older-than", "30d"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        stdout(&out),
        "horizon:   30d (2592000s)\npurged:    1\nunstamped: 1\nconflicts: 0\n"
    );

    assert!(
        !record_file(root.path(), "ns", "col", "old.md").exists(),
        "purged"
    );
    assert!(
        record_file(root.path(), "ns", "col", "young.md").is_file(),
        "too young"
    );
    assert!(
        record_file(root.path(), "ns", "col", "unstamped.md").is_file(),
        "unstamped"
    );
    assert!(
        run(root.path(), &["get", "ns", "col", "live.md"])
            .status
            .success(),
        "live untouched"
    );
}
