# Gonzalo M1 — Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the Gonzalo Cargo workspace and the `caliban-ai/gonzalo` repo, and implement the foundation stack — a generic versioned `Record`/`Store` core with optimistic-concurrency conflict surfacing, a filesystem substrate that passes a shared conformance suite, typed domain views for caliban's data, and a facade crate — giving local parity behind the new abstractions.

**Architecture:** A generic `Record` is the universal persisted unit. `gonzalo-core` defines the `Store` trait (conditional, version-checked writes returning a typed `PutResult` that surfaces conflicts) plus `RecordKind`-keyed merge strategies, with no I/O. `gonzalo-store-fs` implements `Store` over the local filesystem and is validated by a conformance suite shipped from `gonzalo-core`. `gonzalo-domain` maps caliban's typed data (memory tiers, topics, sessions, checkpoints) onto records via serde. `gonzalo` re-exports a curated surface. See `docs/superpowers/specs/2026-06-05-gonzalo-design.md`.

**Tech Stack:** Rust 2024 (1.95), `tokio`, `async-trait`, `serde`/`serde_json`, `blake3` (content hashing), `thiserror`, `tempfile` + `tokio` test harness. License AGPL-3.0-only.

---

## File Structure

Workspace root:
- `Cargo.toml` — `[workspace]` with members + shared `[workspace.package]`, `[workspace.dependencies]`, `[workspace.lints]`.
- `rust-toolchain.toml` — pin `1.95.0`.
- `rustfmt.toml` — formatting config (mirror caliban).
- `LICENSE` — AGPL-3.0-only text.
- `README.md` — project intro + build instructions.
- `.gitignore` — already present (`/target`, `**/*.rs.bk`, `.DS_Store`).

`crates/gonzalo-core/` (no I/O):
- `src/lib.rs` — module wiring + curated `pub use`.
- `src/error.rs` — `CoreError`, `Result`.
- `src/identity.rs` — `Identity`.
- `src/key.rs` — `RecordKey`, `KeyPrefix`.
- `src/revision.rs` — `ContentHash`, `Revision`.
- `src/record.rs` — `RecordKind`, `MergeClass`, `Body`, `Meta`, `Record`.
- `src/store.rs` — `Store` trait, `PutResult`, `Conflict`.
- `src/merge.rs` — `merge` dispatch by `MergeClass`.
- `src/conformance.rs` — reusable `Store` conformance suite (feature `conformance`).

`crates/gonzalo-store-fs/`:
- `src/lib.rs` — `FsStore` implementing `Store`.
- `src/layout.rs` — on-disk path mapping + atomic write helper.
- `tests/conformance.rs` — runs the shared suite against `FsStore`.

`crates/gonzalo-domain/`:
- `src/lib.rs` — module wiring.
- `src/memory.rs` — `MemoryTier`, `Topic` typed views ↔ `Record`.
- `src/session.rs` — `Session` typed view ↔ `Record`.
- `src/checkpoint.rs` — `Checkpoint` typed view ↔ `Record`.
- `src/codec.rs` — `RecordCodec` trait: typed struct ↔ `Body` via serde_json.

`crates/gonzalo/` (facade):
- `src/lib.rs` — curated `pub use` re-exports + feature flags.

---

## Task 1: Scaffold the workspace

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`, `LICENSE`, `README.md`
- Create: `crates/gonzalo-core/Cargo.toml`, `crates/gonzalo-core/src/lib.rs`
- Create: `crates/gonzalo-store-fs/Cargo.toml`, `crates/gonzalo-store-fs/src/lib.rs`
- Create: `crates/gonzalo-domain/Cargo.toml`, `crates/gonzalo-domain/src/lib.rs`
- Create: `crates/gonzalo/Cargo.toml`, `crates/gonzalo/src/lib.rs`

- [ ] **Step 1: Write `rust-toolchain.toml`**

```toml
[toolchain]
channel = "1.95.0"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 2: Write `rustfmt.toml`**

```toml
edition = "2024"
max_width = 100
```

- [ ] **Step 3: Write the root `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = [
    "crates/gonzalo-core",
    "crates/gonzalo-store-fs",
    "crates/gonzalo-domain",
    "crates/gonzalo",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "AGPL-3.0-only"
authors = ["John Ford <john.ford2002@gmail.com>"]
rust-version = "1.95"
repository = "https://github.com/caliban-ai/gonzalo"

[workspace.dependencies]
gonzalo-core   = { path = "crates/gonzalo-core" }
gonzalo-store-fs = { path = "crates/gonzalo-store-fs" }
gonzalo-domain = { path = "crates/gonzalo-domain" }
tokio       = { version = "1", features = ["full"] }
async-trait = "0.1"
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
thiserror   = "1"
blake3      = "1"
tempfile    = "3"

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

- [ ] **Step 4: Write `LICENSE`**

Fetch the canonical AGPL-3.0 text and write it verbatim:

Run: `curl -fsSL https://www.gnu.org/licenses/agpl-3.0.txt -o LICENSE`
Expected: a `LICENSE` file beginning with "GNU AFFERO GENERAL PUBLIC LICENSE".

- [ ] **Step 5: Write `README.md`**

```markdown
# gonzalo

A robust, shareable persistence layer for [caliban](https://github.com/caliban-ai/caliban).

Gonzalo lifts caliban's local-first state — memory tiers, auto-memory topics,
sessions, and checkpoints — into a layer that can be shared across multiple
systems and contributors, via pluggable storage substrates behind a generic,
versioned, conflict-aware core. See `docs/superpowers/specs/` for the design.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).

## Building

```bash
cargo build --workspace
cargo test  --workspace
```
```

- [ ] **Step 6: Write each crate's `Cargo.toml`**

`crates/gonzalo-core/Cargo.toml`:
```toml
[package]
name = "gonzalo-core"
description = "Generic versioned Record/Store core for gonzalo"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
async-trait = { workspace = true }
serde       = { workspace = true }
serde_json  = { workspace = true }
thiserror   = { workspace = true }
blake3      = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }

[features]
# Exposes the reusable Store conformance suite to other crates' tests.
conformance = []

[lints]
workspace = true
```

`crates/gonzalo-store-fs/Cargo.toml`:
```toml
[package]
name = "gonzalo-store-fs"
description = "Filesystem storage substrate for gonzalo"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
gonzalo-core = { workspace = true }
async-trait  = { workspace = true }
serde_json   = { workspace = true }
tokio        = { workspace = true, features = ["fs", "io-util", "rt", "macros"] }
thiserror    = { workspace = true }

[dev-dependencies]
gonzalo-core = { workspace = true, features = ["conformance"] }
tokio        = { workspace = true }
tempfile     = { workspace = true }

[lints]
workspace = true
```

`crates/gonzalo-domain/Cargo.toml`:
```toml
[package]
name = "gonzalo-domain"
description = "Typed domain views over gonzalo records (memory, sessions, checkpoints)"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
gonzalo-core = { workspace = true }
serde        = { workspace = true }
serde_json   = { workspace = true }
thiserror    = { workspace = true }

[lints]
workspace = true
```

`crates/gonzalo/Cargo.toml`:
```toml
[package]
name = "gonzalo"
description = "Facade for the gonzalo persistence layer"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
gonzalo-core   = { workspace = true }
gonzalo-domain = { workspace = true }
gonzalo-store-fs = { workspace = true, optional = true }

[features]
default = ["fs"]
fs = ["dep:gonzalo-store-fs"]

[lints]
workspace = true
```

- [ ] **Step 7: Write placeholder `lib.rs` for each crate**

`crates/gonzalo-core/src/lib.rs`, `crates/gonzalo-store-fs/src/lib.rs`, `crates/gonzalo-domain/src/lib.rs`, `crates/gonzalo/src/lib.rs` each containing exactly:
```rust
//! Placeholder; populated by subsequent tasks.
```

- [ ] **Step 8: Verify the workspace builds**

Run: `cargo build --workspace`
Expected: compiles cleanly (four empty library crates).

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "chore: scaffold gonzalo cargo workspace"
```

---

## Task 2: Create the GitHub repository and push

**Files:** none (git/remote operations only).

- [ ] **Step 1: Create the private repo under caliban-ai**

Run:
```bash
gh repo create caliban-ai/gonzalo \
  --private \
  --description "Robust, shareable persistence layer for caliban" \
  --source . --remote origin
```
Expected: repo created, `origin` remote added pointing at `git@github.com:caliban-ai/gonzalo.git` (or https).

> Note: created **private** to match caliban's posture ("Private repo, designed to be open-sourced"). If you want it public from day one, add `--public` instead.

- [ ] **Step 2: Push `main` and set upstream**

Run: `git push -u origin main`
Expected: `main` published; subsequent commits track `origin/main`.

- [ ] **Step 3: Verify**

Run: `gh repo view caliban-ai/gonzalo --json name,visibility,defaultBranchRef`
Expected: name `gonzalo`, the chosen visibility, default branch `main`.

---

## Task 3: `gonzalo-core` — Identity, RecordKey, KeyPrefix

**Files:**
- Create: `crates/gonzalo-core/src/identity.rs`
- Create: `crates/gonzalo-core/src/key.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`

- [ ] **Step 1: Write the failing test** (append to `crates/gonzalo-core/src/key.rs`)

```rust
//! Stable addressing for records.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The stable address of a record: `namespace/collection/id`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordKey {
    pub namespace: String,
    pub collection: String,
    pub id: String,
}

impl RecordKey {
    pub fn new(
        namespace: impl Into<String>,
        collection: impl Into<String>,
        id: impl Into<String>,
    ) -> Self {
        Self { namespace: namespace.into(), collection: collection.into(), id: id.into() }
    }
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.namespace, self.collection, self.id)
    }
}

/// A prefix used to list records. `None` fields match anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyPrefix {
    pub namespace: Option<String>,
    pub collection: Option<String>,
}

impl KeyPrefix {
    pub fn matches(&self, key: &RecordKey) -> bool {
        self.namespace.as_ref().is_none_or(|n| n == &key.namespace)
            && self.collection.as_ref().is_none_or(|c| c == &key.collection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_slash_joined() {
        let k = RecordKey::new("caliban", "topics", "rust-tips");
        assert_eq!(k.to_string(), "caliban/topics/rust-tips");
    }

    #[test]
    fn prefix_matches_on_set_fields_only() {
        let k = RecordKey::new("caliban", "topics", "x");
        assert!(KeyPrefix { namespace: Some("caliban".into()), collection: None }.matches(&k));
        assert!(!KeyPrefix { namespace: Some("other".into()), collection: None }.matches(&k));
        assert!(KeyPrefix::default().matches(&k));
    }
}
```

- [ ] **Step 2: Write `identity.rs`**

```rust
//! Contributor identity attached to every write.

use serde::{Deserialize, Serialize};

/// Who made a change. In local mode this is a configured local identity;
/// in daemon mode the server authenticates it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub display: Option<String>,
}

impl Identity {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), display: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_id_and_no_display() {
        let i = Identity::new("john");
        assert_eq!(i.id, "john");
        assert_eq!(i.display, None);
    }
}
```

- [ ] **Step 3: Wire modules in `lib.rs`**

```rust
//! Generic versioned Record/Store core for gonzalo.

pub mod identity;
pub mod key;

pub use identity::Identity;
pub use key::{KeyPrefix, RecordKey};
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p gonzalo-core`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-core
git commit -m "feat(core): add Identity, RecordKey, KeyPrefix"
```

---

## Task 4: `gonzalo-core` — ContentHash and Revision

**Files:**
- Create: `crates/gonzalo-core/src/revision.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`

- [ ] **Step 1: Write the failing test + impl** (`crates/gonzalo-core/src/revision.rs`)

```rust
//! Content hashing and per-record revisions for optimistic concurrency.

use serde::{Deserialize, Serialize};

/// A content hash (blake3, hex-encoded) of a record body.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }
}

/// A record revision: a monotonic counter plus the body's content hash.
/// Two writers diverge when their `counter`/`hash` pair differs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub counter: u64,
    pub hash: ContentHash,
}

impl Revision {
    /// The first revision for a freshly created record body.
    pub fn initial(body: &[u8]) -> Self {
        Self { counter: 0, hash: ContentHash::of(body) }
    }

    /// The next revision after `self` for an updated body.
    pub fn next(&self, body: &[u8]) -> Self {
        Self { counter: self.counter + 1, hash: ContentHash::of(body) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_distinct() {
        assert_eq!(ContentHash::of(b"abc"), ContentHash::of(b"abc"));
        assert_ne!(ContentHash::of(b"abc"), ContentHash::of(b"abd"));
    }

    #[test]
    fn next_increments_counter_and_rehashes() {
        let r0 = Revision::initial(b"v1");
        let r1 = r0.next(b"v2");
        assert_eq!(r0.counter, 0);
        assert_eq!(r1.counter, 1);
        assert_ne!(r0.hash, r1.hash);
    }
}
```

- [ ] **Step 2: Wire module in `lib.rs`** (add lines)

```rust
pub mod revision;
pub use revision::{ContentHash, Revision};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p gonzalo-core`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-core
git commit -m "feat(core): add ContentHash and Revision"
```

---

## Task 5: `gonzalo-core` — Record, RecordKind, MergeClass, Body, Meta

**Files:**
- Create: `crates/gonzalo-core/src/record.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`

- [ ] **Step 1: Write the failing test + impl** (`crates/gonzalo-core/src/record.rs`)

```rust
//! The universal persisted unit and its classification.

use crate::{Identity, RecordKey, Revision};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a record represents. Drives the merge strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordKind {
    MemoryTier,
    Topic,
    Session,
    Checkpoint,
}

/// How concurrent edits to a record of a given kind are reconciled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeClass {
    /// Edits union/concatenate (auto-memory topics, session transcripts).
    AppendOnly,
    /// Field-level 3-way merge against the common base.
    Structured,
    /// No safe automatic merge; surface to the caller.
    Opaque,
}

impl RecordKind {
    pub fn merge_class(self) -> MergeClass {
        match self {
            RecordKind::Topic | RecordKind::Session => MergeClass::AppendOnly,
            RecordKind::MemoryTier => MergeClass::Structured,
            RecordKind::Checkpoint => MergeClass::Opaque,
        }
    }
}

/// A record body. M1 stores bytes inline; the `Blob` content-addressed
/// variant is reserved for M2 (large session/checkpoint externalization).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    Inline(Vec<u8>),
}

impl Body {
    /// The bytes used for content hashing and merging.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Body::Inline(b) => b,
        }
    }
}

/// Provenance and labels for a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub author: Identity,
    pub origin_system: String,
    pub created: i64,
    pub updated: i64,
    pub labels: BTreeMap<String, String>,
}

/// The universal persisted unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub key: RecordKey,
    pub kind: RecordKind,
    pub revision: Revision,
    pub parent: Option<Revision>,
    pub body: Body,
    pub meta: Meta,
    pub links: Vec<RecordKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_class_is_assigned_per_kind() {
        assert_eq!(RecordKind::Topic.merge_class(), MergeClass::AppendOnly);
        assert_eq!(RecordKind::Session.merge_class(), MergeClass::AppendOnly);
        assert_eq!(RecordKind::MemoryTier.merge_class(), MergeClass::Structured);
        assert_eq!(RecordKind::Checkpoint.merge_class(), MergeClass::Opaque);
    }

    #[test]
    fn body_exposes_bytes() {
        assert_eq!(Body::Inline(b"hi".to_vec()).bytes(), b"hi");
    }
}
```

- [ ] **Step 2: Wire module in `lib.rs`** (add lines)

```rust
pub mod record;
pub use record::{Body, MergeClass, Meta, Record, RecordKind};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p gonzalo-core`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-core
git commit -m "feat(core): add Record, RecordKind, MergeClass, Body, Meta"
```

---

## Task 6: `gonzalo-core` — error type

**Files:**
- Create: `crates/gonzalo-core/src/error.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`

- [ ] **Step 1: Write `error.rs`**

```rust
//! Core error type. Note: write *conflicts* are NOT errors — they are a
//! typed `PutResult` variant (see `store.rs`). Errors here are genuine
//! failures (I/O, serialization, missing parent for an update).

use crate::RecordKey;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("record not found: {0}")]
    NotFound(RecordKey),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
```

- [ ] **Step 2: Wire module in `lib.rs`** (add lines)

```rust
pub mod error;
pub use error::{CoreError, Result};
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p gonzalo-core`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-core
git commit -m "feat(core): add CoreError"
```

---

## Task 7: `gonzalo-core` — Store trait, PutResult, Conflict

**Files:**
- Create: `crates/gonzalo-core/src/store.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`

- [ ] **Step 1: Write `store.rs`**

```rust
//! The generic storage substrate trait and write-outcome types.

use crate::{Record, RecordKey, Revision, Result};
use async_trait::async_trait;

/// A detected concurrent-edit conflict: the caller's write expected
/// `expected` but the store holds `current`. `base` is the common ancestor
/// revision if known. Surfaced, never silently resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub key: RecordKey,
    pub expected: Option<Revision>,
    pub current: Record,
}

/// The outcome of a conditional write. `Conflict` is a normal, recoverable
/// result — not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PutResult {
    Committed(Revision),
    Conflict(Conflict),
}

/// A pluggable storage substrate over generic records.
#[async_trait]
pub trait Store: Send + Sync {
    /// Fetch a record by key, or `None` if absent.
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>>;

    /// Conditionally write `record`. `expected` is the revision the caller
    /// believes is current (`None` means "expect no existing record").
    /// If the store's current revision differs, returns `PutResult::Conflict`.
    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>;

    /// List keys matching `prefix`.
    async fn list(&self, prefix: &crate::KeyPrefix) -> Result<Vec<RecordKey>>;
}
```

- [ ] **Step 2: Wire module in `lib.rs`** (add lines)

```rust
pub mod store;
pub use store::{Conflict, PutResult, Store};
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p gonzalo-core`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-core
git commit -m "feat(core): add Store trait, PutResult, Conflict"
```

---

## Task 8: `gonzalo-core` — merge strategies

**Files:**
- Create: `crates/gonzalo-core/src/merge.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`

- [ ] **Step 1: Write the failing test + impl** (`crates/gonzalo-core/src/merge.rs`)

```rust
//! Merge strategies keyed by `MergeClass`. Used by `Sync` (M2) and by
//! callers resolving a `PutResult::Conflict`.

use crate::record::{Body, MergeClass};

/// The result of attempting an automatic merge of two divergent bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// A merged body was produced automatically.
    Merged(Body),
    /// No safe automatic merge; the caller must resolve.
    NeedsResolution,
}

/// Attempt to merge `ours` and `theirs` given their common `base`,
/// according to `class`.
///
/// - `AppendOnly`: union of lines from base→ours and base→theirs, in a
///   stable order (base lines, then new ours lines, then new theirs lines),
///   de-duplicated. Suits append-only topics and transcripts.
/// - `Structured`: deferred to M2 (returns `NeedsResolution` for now).
/// - `Opaque`: always `NeedsResolution`.
pub fn merge(class: MergeClass, base: &Body, ours: &Body, theirs: &Body) -> MergeOutcome {
    match class {
        MergeClass::AppendOnly => append_only_merge(base, ours, theirs),
        MergeClass::Structured | MergeClass::Opaque => MergeOutcome::NeedsResolution,
    }
}

fn append_only_merge(base: &Body, ours: &Body, theirs: &Body) -> MergeOutcome {
    let base_lines: Vec<&[u8]> = split_lines(base.bytes());
    let ours_new = new_lines(&base_lines, ours.bytes());
    let theirs_new = new_lines(&base_lines, theirs.bytes());

    let mut out: Vec<u8> = Vec::new();
    let mut push = |line: &[u8], out: &mut Vec<u8>| {
        out.extend_from_slice(line);
        out.push(b'\n');
    };
    for line in &base_lines {
        push(line, &mut out);
    }
    for line in ours_new.iter().chain(theirs_new.iter()) {
        push(line, &mut out);
    }
    MergeOutcome::Merged(Body::Inline(out))
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect()
}

/// Lines present in `bytes` but not in `base_lines`, preserving order and
/// dropping duplicates.
fn new_lines<'a>(base_lines: &[&[u8]], bytes: &'a [u8]) -> Vec<&'a [u8]> {
    let mut seen: Vec<&[u8]> = base_lines.to_vec();
    let mut out = Vec::new();
    for line in split_lines(bytes) {
        if !seen.contains(&line) {
            seen.push(line);
            out.push(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(s: &str) -> Body {
        Body::Inline(s.as_bytes().to_vec())
    }

    #[test]
    fn append_only_unions_disjoint_additions() {
        let base = body("a\n");
        let ours = body("a\nb\n");
        let theirs = body("a\nc\n");
        let MergeOutcome::Merged(m) = merge(MergeClass::AppendOnly, &base, &ours, &theirs) else {
            panic!("expected merge");
        };
        assert_eq!(m, body("a\nb\nc\n"));
    }

    #[test]
    fn append_only_dedups_same_addition() {
        let base = body("a\n");
        let ours = body("a\nb\n");
        let theirs = body("a\nb\n");
        let MergeOutcome::Merged(m) = merge(MergeClass::AppendOnly, &base, &ours, &theirs) else {
            panic!("expected merge");
        };
        assert_eq!(m, body("a\nb\n"));
    }

    #[test]
    fn opaque_needs_resolution() {
        let b = body("x\n");
        assert_eq!(merge(MergeClass::Opaque, &b, &b, &b), MergeOutcome::NeedsResolution);
    }
}
```

- [ ] **Step 2: Wire module in `lib.rs`** (add lines)

```rust
pub mod merge;
pub use merge::{MergeOutcome, merge};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p gonzalo-core`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-core
git commit -m "feat(core): add merge strategies keyed by MergeClass"
```

---

## Task 9: `gonzalo-core` — Store conformance suite

**Files:**
- Create: `crates/gonzalo-core/src/conformance.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`

- [ ] **Step 1: Write `conformance.rs`** (gated behind the `conformance` feature)

```rust
//! A reusable conformance suite every `Store` impl must pass. Substrate
//! crates call `run_store_conformance(factory)` from their integration
//! tests. The factory returns a fresh, empty store per invocation.

use crate::{
    Body, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind, Revision, Store,
};
use std::collections::BTreeMap;

fn sample(key: RecordKey, payload: &[u8]) -> Record {
    let body = Body::Inline(payload.to_vec());
    Record {
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        kind: RecordKind::Topic,
        meta: Meta {
            author: Identity::new("tester"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        key,
    }
}

/// Run the full suite against a store produced by `factory`.
pub async fn run_store_conformance<S, F, Fut>(factory: F)
where
    S: Store,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = S>,
{
    get_absent_returns_none(&factory().await).await;
    put_then_get_roundtrips(&factory().await).await;
    stale_expected_returns_conflict(&factory().await).await;
    list_filters_by_prefix(&factory().await).await;
}

async fn get_absent_returns_none<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "missing");
    assert_eq!(store.get(&key).await.unwrap(), None);
}

async fn put_then_get_roundtrips<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "a");
    let rec = sample(key.clone(), b"hello");
    let out = store.put(rec.clone(), None).await.unwrap();
    assert!(matches!(out, PutResult::Committed(_)));
    assert_eq!(store.get(&key).await.unwrap(), Some(rec));
}

async fn stale_expected_returns_conflict<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "b");
    let first = sample(key.clone(), b"v1");
    let committed = match store.put(first.clone(), None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };

    // A second writer who never saw `committed` tries to create again.
    let stale = sample(key.clone(), b"v2-from-stale-writer");
    match store.put(stale, None).await.unwrap() {
        PutResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, committed);
        }
        PutResult::Committed(_) => panic!("expected conflict for stale write"),
    }
}

async fn list_filters_by_prefix<S: Store>(store: &S) {
    store.put(sample(RecordKey::new("x", "c1", "1"), b"1"), None).await.unwrap();
    store.put(sample(RecordKey::new("x", "c2", "2"), b"2"), None).await.unwrap();
    let prefix = KeyPrefix { namespace: Some("x".into()), collection: Some("c1".into()) };
    let mut keys = store.list(&prefix).await.unwrap();
    keys.sort();
    assert_eq!(keys, vec![RecordKey::new("x", "c1", "1")]);
}
```

- [ ] **Step 2: Wire module in `lib.rs`** (add at end)

```rust
#[cfg(feature = "conformance")]
pub mod conformance;
```

- [ ] **Step 3: Verify it compiles under the feature**

Run: `cargo build -p gonzalo-core --features conformance`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-core
git commit -m "feat(core): add reusable Store conformance suite"
```

---

## Task 10: `gonzalo-store-fs` — filesystem layout

**Files:**
- Create: `crates/gonzalo-store-fs/src/layout.rs`
- Modify: `crates/gonzalo-store-fs/src/lib.rs`

- [ ] **Step 1: Write the failing test + impl** (`crates/gonzalo-store-fs/src/layout.rs`)

```rust
//! On-disk path mapping. Each record is one JSON file at
//! `<root>/<namespace>/<collection>/<id>.json`. Components are percent-ish
//! sanitized so arbitrary ids cannot escape the root.

use gonzalo_core::RecordKey;
use std::path::{Path, PathBuf};

/// Encode a key component so it is a single safe path segment.
fn seg(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// The file path for a record's JSON under `root`.
pub fn record_path(root: &Path, key: &RecordKey) -> PathBuf {
    root.join(seg(&key.namespace)).join(seg(&key.collection)).join(format!("{}.json", seg(&key.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_path_is_nested_json() {
        let root = Path::new("/tmp/g");
        let key = RecordKey::new("caliban", "topics", "rust");
        assert_eq!(record_path(root, &key), Path::new("/tmp/g/caliban/topics/rust.json"));
    }

    #[test]
    fn unsafe_chars_are_neutralized() {
        let root = Path::new("/tmp/g");
        let key = RecordKey::new("..", "../etc", "../../passwd");
        let p = record_path(root, &key);
        assert!(p.starts_with("/tmp/g"));
        assert!(!p.to_string_lossy().contains(".."));
    }
}
```

- [ ] **Step 2: Set `lib.rs` to declare the module**

```rust
//! Filesystem storage substrate for gonzalo.

mod layout;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p gonzalo-store-fs`
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-store-fs
git commit -m "feat(store-fs): add on-disk record path layout"
```

---

## Task 11: `gonzalo-store-fs` — FsStore implements Store

**Files:**
- Modify: `crates/gonzalo-store-fs/src/lib.rs`

- [ ] **Step 1: Write `FsStore`** (`crates/gonzalo-store-fs/src/lib.rs`)

```rust
//! Filesystem storage substrate for gonzalo.

mod layout;

use async_trait::async_trait;
use gonzalo_core::{
    CoreError, KeyPrefix, PutResult, Record, RecordKey, Result, Store, store::Conflict,
};
use std::path::PathBuf;

/// A `Store` backed by JSON files under a root directory.
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    async fn read_record(&self, key: &RecordKey) -> Result<Option<Record>> {
        let path = layout::record_path(&self.root, key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let rec: Record =
                    serde_json::from_slice(&bytes).map_err(|e| CoreError::Serde(e.to_string()))?;
                Ok(Some(rec))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CoreError::Backend(e.to_string())),
        }
    }
}

#[async_trait]
impl Store for FsStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.read_record(key).await
    }

    async fn put(&self, record: Record, expected: Option<gonzalo_core::Revision>) -> Result<PutResult> {
        // Optimistic concurrency: the stored revision must equal `expected`.
        let current = self.read_record(&record.key).await?;
        let current_rev = current.as_ref().map(|r| r.revision.clone());
        if current_rev != expected {
            if let Some(current) = current {
                return Ok(PutResult::Conflict(Conflict {
                    key: record.key.clone(),
                    expected,
                    current,
                }));
            }
            // expected referenced a revision but nothing exists: treat as conflict
            return Err(CoreError::NotFound(record.key.clone()));
        }

        let path = layout::record_path(&self.root, &record.key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::Backend(e.to_string()))?;
        }
        let bytes =
            serde_json::to_vec_pretty(&record).map_err(|e| CoreError::Serde(e.to_string()))?;
        // Atomic write: temp file + rename.
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &bytes).await.map_err(|e| CoreError::Backend(e.to_string()))?;
        tokio::fs::rename(&tmp, &path).await.map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(PutResult::Committed(record.revision))
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let mut out = Vec::new();
        collect_keys(&self.root, prefix, &mut out).await?;
        Ok(out)
    }
}

/// Walk `<root>/<ns>/<col>/<id>.json` and collect keys matching `prefix`.
async fn collect_keys(root: &std::path::Path, prefix: &KeyPrefix, out: &mut Vec<RecordKey>) -> Result<()> {
    let mut namespaces = match tokio::fs::read_dir(root).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CoreError::Backend(e.to_string())),
    };
    while let Some(ns) = namespaces.next_entry().await.map_err(|e| CoreError::Backend(e.to_string()))? {
        let ns_name = ns.file_name().to_string_lossy().to_string();
        let mut cols = tokio::fs::read_dir(ns.path()).await.map_err(|e| CoreError::Backend(e.to_string()))?;
        while let Some(col) = cols.next_entry().await.map_err(|e| CoreError::Backend(e.to_string()))? {
            let col_name = col.file_name().to_string_lossy().to_string();
            let mut files = tokio::fs::read_dir(col.path()).await.map_err(|e| CoreError::Backend(e.to_string()))?;
            while let Some(f) = files.next_entry().await.map_err(|e| CoreError::Backend(e.to_string()))? {
                let fname = f.file_name().to_string_lossy().to_string();
                if let Some(id) = fname.strip_suffix(".json") {
                    let key = RecordKey::new(ns_name.clone(), col_name.clone(), id.to_string());
                    if prefix.matches(&key) {
                        out.push(key);
                    }
                }
            }
        }
    }
    Ok(())
}
```

> Note on the conformance suite: `read_record` reconstructs the record from
> disk, and `stale_expected_returns_conflict` creates with `expected = None`
> while a record already exists, so `current_rev (Some) != expected (None)`
> yields `PutResult::Conflict` via the `Some(current)` branch. Verify this in
> Task 12.

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p gonzalo-store-fs`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/gonzalo-store-fs
git commit -m "feat(store-fs): implement Store for FsStore"
```

---

## Task 12: `gonzalo-store-fs` — run the conformance suite

**Files:**
- Create: `crates/gonzalo-store-fs/tests/conformance.rs`

- [ ] **Step 1: Write the integration test**

```rust
use gonzalo_core::conformance::run_store_conformance;
use gonzalo_store_fs::FsStore;

#[tokio::test]
async fn fs_store_passes_conformance() {
    run_store_conformance(|| async {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so the directory survives for the store's lifetime
        // within a single factory invocation; the OS reclaims /tmp on reboot.
        let path = dir.keep();
        FsStore::new(path)
    })
    .await;
}
```

> `tempfile::TempDir::keep` (formerly `into_path`) returns the `PathBuf` and
> disables auto-deletion, which is what we want: each factory call gets its
> own fresh directory.

- [ ] **Step 2: Run the conformance test**

Run: `cargo test -p gonzalo-store-fs --test conformance`
Expected: PASS — `fs_store_passes_conformance`.

- [ ] **Step 3: Commit**

```bash
git add crates/gonzalo-store-fs
git commit -m "test(store-fs): pass the core Store conformance suite"
```

---

## Task 13: `gonzalo-domain` — RecordCodec

**Files:**
- Create: `crates/gonzalo-domain/src/codec.rs`
- Modify: `crates/gonzalo-domain/src/lib.rs`

- [ ] **Step 1: Write the failing test + impl** (`crates/gonzalo-domain/src/codec.rs`)

```rust
//! Mapping between typed domain structs and generic record bodies.

use gonzalo_core::{Body, CoreError, Result};
use serde::{Serialize, de::DeserializeOwned};

/// A typed value that can be stored in a record body as JSON.
pub trait RecordCodec: Serialize + DeserializeOwned {
    fn to_body(&self) -> Result<Body> {
        let bytes = serde_json::to_vec(self).map_err(|e| CoreError::Serde(e.to_string()))?;
        Ok(Body::Inline(bytes))
    }

    fn from_body(body: &Body) -> Result<Self> {
        serde_json::from_slice(body.bytes()).map_err(|e| CoreError::Serde(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Demo {
        n: u32,
        s: String,
    }
    impl RecordCodec for Demo {}

    #[test]
    fn roundtrips_through_body() {
        let d = Demo { n: 7, s: "x".into() };
        let body = d.to_body().unwrap();
        assert_eq!(Demo::from_body(&body).unwrap(), d);
    }
}
```

- [ ] **Step 2: Set `lib.rs` to declare the module**

```rust
//! Typed domain views over gonzalo records.

pub mod codec;
pub use codec::RecordCodec;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p gonzalo-domain`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-domain
git commit -m "feat(domain): add RecordCodec for typed body mapping"
```

---

## Task 14: `gonzalo-domain` — typed views (memory, session, checkpoint)

**Files:**
- Create: `crates/gonzalo-domain/src/memory.rs`
- Create: `crates/gonzalo-domain/src/session.rs`
- Create: `crates/gonzalo-domain/src/checkpoint.rs`
- Modify: `crates/gonzalo-domain/src/lib.rs`

- [ ] **Step 1: Write `memory.rs`**

```rust
//! Memory-tier and auto-memory topic views.

use crate::codec::RecordCodec;
use gonzalo_core::RecordKind;
use serde::{Deserialize, Serialize};

/// A CLAUDE.md-style memory tier file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryTier {
    pub name: String,
    pub content: String,
}
impl RecordCodec for MemoryTier {}
impl MemoryTier {
    pub const KIND: RecordKind = RecordKind::MemoryTier;
}

/// An auto-memory topic: a slug plus append-only bullet lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topic {
    pub slug: String,
    pub bullets: Vec<String>,
}
impl RecordCodec for Topic {}
impl Topic {
    pub const KIND: RecordKind = RecordKind::Topic;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::RecordCodec;

    #[test]
    fn tier_roundtrips() {
        let t = MemoryTier { name: "global".into(), content: "be concise".into() };
        assert_eq!(MemoryTier::from_body(&t.to_body().unwrap()).unwrap(), t);
        assert_eq!(MemoryTier::KIND, RecordKind::MemoryTier);
    }

    #[test]
    fn topic_roundtrips() {
        let t = Topic { slug: "rust".into(), bullets: vec!["use clippy".into()] };
        assert_eq!(Topic::from_body(&t.to_body().unwrap()).unwrap(), t);
        assert_eq!(Topic::KIND, RecordKind::Topic);
    }
}
```

- [ ] **Step 2: Write `session.rs`**

```rust
//! Session (conversation transcript) view.

use crate::codec::RecordCodec;
use gonzalo_core::RecordKind;
use serde::{Deserialize, Serialize};

/// One transcript turn (role + text). Kept deliberately minimal for M1;
/// richer turn modeling tracks caliban's session schema in a later milestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub text: String,
}

/// A conversation session: an ordered, append-only list of turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub turns: Vec<Turn>,
}
impl RecordCodec for Session {}
impl Session {
    pub const KIND: RecordKind = RecordKind::Session;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::RecordCodec;

    #[test]
    fn session_roundtrips() {
        let s = Session {
            name: "research".into(),
            turns: vec![Turn { role: "user".into(), text: "hi".into() }],
        };
        assert_eq!(Session::from_body(&s.to_body().unwrap()).unwrap(), s);
        assert_eq!(Session::KIND, RecordKind::Session);
    }
}
```

- [ ] **Step 3: Write `checkpoint.rs`**

```rust
//! Checkpoint view (opaque snapshot blob + label).

use crate::codec::RecordCodec;
use gonzalo_core::RecordKind;
use serde::{Deserialize, Serialize};

/// A checkpoint: a labeled, opaque snapshot payload (base64 or raw text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub label: String,
    pub payload: String,
}
impl RecordCodec for Checkpoint {}
impl Checkpoint {
    pub const KIND: RecordKind = RecordKind::Checkpoint;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::RecordCodec;

    #[test]
    fn checkpoint_roundtrips() {
        let c = Checkpoint { label: "before-refactor".into(), payload: "blob".into() };
        assert_eq!(Checkpoint::from_body(&c.to_body().unwrap()).unwrap(), c);
        assert_eq!(Checkpoint::KIND, RecordKind::Checkpoint);
    }
}
```

- [ ] **Step 4: Update `lib.rs`**

```rust
//! Typed domain views over gonzalo records.

pub mod checkpoint;
pub mod codec;
pub mod memory;
pub mod session;

pub use checkpoint::Checkpoint;
pub use codec::RecordCodec;
pub use memory::{MemoryTier, Topic};
pub use session::{Session, Turn};
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p gonzalo-domain`
Expected: PASS (5 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-domain
git commit -m "feat(domain): add memory, session, checkpoint typed views"
```

---

## Task 15: `gonzalo` facade — curated re-exports

**Files:**
- Modify: `crates/gonzalo/src/lib.rs`

- [ ] **Step 1: Write the facade**

```rust
//! gonzalo — a robust, shareable persistence layer for caliban.
//!
//! This facade re-exports the curated surface most consumers need and
//! selects storage substrates via Cargo features (`fs` is on by default).

pub use gonzalo_core::{
    Body, Conflict, ContentHash, CoreError, Identity, KeyPrefix, MergeClass, MergeOutcome, Meta,
    PutResult, Record, RecordKey, RecordKind, Result, Revision, Store, merge,
};

pub use gonzalo_domain::{Checkpoint, MemoryTier, RecordCodec, Session, Topic, Turn};

#[cfg(feature = "fs")]
pub use gonzalo_store_fs::FsStore;
```

- [ ] **Step 2: Write a facade smoke test** (`crates/gonzalo/src/lib.rs`, append)

```rust
#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn end_to_end_put_get_via_facade() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        let topic = Topic { slug: "rust".into(), bullets: vec!["use clippy".into()] };
        let body = topic.to_body().unwrap();
        let key = RecordKey::new("caliban", "topics", "rust");
        let rec = Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind: Topic::KIND,
            meta: Meta {
                author: Identity::new("john"),
                origin_system: "laptop".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            key: key.clone(),
        };

        assert!(matches!(store.put(rec, None).await.unwrap(), PutResult::Committed(_)));
        let got = store.get(&key).await.unwrap().unwrap();
        assert_eq!(Topic::from_body(&got.body).unwrap(), topic);
    }
}
```

- [ ] **Step 3: Add `tokio` + `tempfile` as dev-deps for the facade** (`crates/gonzalo/Cargo.toml`, add section)

```toml
[dev-dependencies]
tokio    = { workspace = true }
tempfile = { workspace = true }
```

- [ ] **Step 4: Run the facade test**

Run: `cargo test -p gonzalo`
Expected: PASS — `end_to_end_put_get_via_facade`.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo
git commit -m "feat(gonzalo): add facade re-exports and end-to-end smoke test"
```

---

## Task 16: Workspace-wide verification and push

**Files:** none.

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: no diffs (fix with `cargo fmt --all` and re-commit if needed).

- [ ] **Step 2: Clippy across the workspace**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Full test run**

Run: `cargo test --workspace`
Expected: all tests PASS, including the fs conformance test and the facade smoke test.

- [ ] **Step 4: Build with the conformance feature too**

Run: `cargo build --workspace --features gonzalo-core/conformance`
Expected: compiles.

- [ ] **Step 5: Commit any fmt/clippy fixes and push**

```bash
git add -A
git commit -m "chore: workspace fmt + clippy clean" || echo "nothing to commit"
git push
```

---

## Self-Review

**Spec coverage (M1 portion of `2026-06-05-gonzalo-design.md`):**
- §1 library consumption, AGPL, Rust 2024/1.95 → Tasks 1, 2. (Daemon consumption is M3.)
- §2 layered architecture, Approach A generic core → Tasks 3–15.
- §3 crates `gonzalo-core`, `gonzalo-store-fs`, `gonzalo-domain`, `gonzalo` → Tasks 1, 3–15. (Other 8 crates are later milestones.)
- §4 `Record`/`RecordKey`/`Revision`/`Body`/`Meta`/`Identity` → Tasks 3–5.
- §5 versioned + OCC + conflict surfacing + merge-by-kind → Tasks 7, 8, 11.
- §10 errors (`thiserror`, Conflict as non-error), substrate conformance suite → Tasks 6, 9, 12.
- §11 fs mirrors caliban layout, facade is caliban's single dependency → Tasks 10, 15. (Full caliban swap + `migrate` is M6.)
- Deferred to later milestones (correctly out of scope here): §6 Sync, §7 vector, §8 graph, §9 daemon/auth, §3 remaining crates.

**Placeholder scan:** No "TBD"/"implement later". `Body::Blob` and `Structured` merge are explicitly deferred to M2 with `NeedsResolution`/`Inline`-only behavior that compiles and is tested today — not placeholders, but scoped M1 behavior.

**Type consistency:** `RecordKey::new`, `Revision::initial/next`, `Body::Inline`/`bytes()`, `PutResult::{Committed,Conflict}`, `Conflict { key, expected, current }`, `Store::{get,put,list}`, `RecordCodec::{to_body,from_body}`, and each domain type's `KIND` const are defined once and used consistently across Tasks 9, 11, 12, 14, 15.

**Note on a known sharp edge:** Task 11's `put` treats "`expected = Some(rev)` but no record exists" as `CoreError::NotFound`. The conformance suite (Task 9) only exercises `expected = None`; this NotFound branch is asserted indirectly via the facade/manual flows. If M2's Sync needs create-if-absent-with-expected semantics, revisit this branch then.
