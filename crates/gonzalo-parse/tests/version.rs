//! The worker reports the extraction format it produces (#228).
//!
//! `EXTRACTION_VERSION` is compiled into whoever asks, but the worker is a
//! separately installed binary — so the indexer cannot assume its own constant
//! describes what actually parsed. These cover the probe it uses to ask.

use gonzalo_graph::EXTRACTION_VERSION;
use gonzalo_parse::worker_extraction_version;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Generous next to a process spawn, and never reached by a healthy worker.
const PROBE: Duration = Duration::from_secs(5);

fn worker_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gonzalo-parse-worker"))
}

/// Write an executable `#!/bin/sh` script standing in for a worker binary.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn the_real_worker_reports_the_version_it_was_built_with() {
    assert_eq!(
        worker_extraction_version(&worker_bin(), PROBE).await,
        Some(EXTRACTION_VERSION),
    );
}

#[tokio::test]
async fn a_worker_that_cannot_report_reads_as_unknown() {
    // A pre-#228 worker ignores argv and blocks reading stdin. The probe closes
    // stdin, so such a worker exits silently — which must read as "unknown",
    // never as agreement with whatever the caller happens to expect.
    let dir = tempfile::tempdir().unwrap();
    let old = script(
        dir.path(),
        "old-worker",
        "#!/bin/sh\nwhile IFS= read -r _; do :; done\n",
    );
    assert_eq!(worker_extraction_version(&old, PROBE).await, None);
}

#[tokio::test]
async fn a_worker_reporting_an_older_version_is_read_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let old = script(
        dir.path(),
        "v1-worker",
        "#!/bin/sh\n[ \"$1\" = --extraction-version ] && echo 1\n",
    );
    assert_eq!(worker_extraction_version(&old, PROBE).await, Some(1));
}

#[tokio::test]
async fn a_missing_worker_reads_as_unknown() {
    assert_eq!(
        worker_extraction_version(Path::new("/nonexistent/gonzalo-parse-worker"), PROBE).await,
        None,
    );
}

#[tokio::test]
async fn a_worker_that_hangs_on_the_probe_reads_as_unknown() {
    // The probe must not be able to wedge an index run.
    let dir = tempfile::tempdir().unwrap();
    let hung = script(dir.path(), "hung-worker", "#!/bin/sh\nsleep 60\n");
    assert_eq!(
        worker_extraction_version(&hung, Duration::from_millis(200)).await,
        None
    );
}
