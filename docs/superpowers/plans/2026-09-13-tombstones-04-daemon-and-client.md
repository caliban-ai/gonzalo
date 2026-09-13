# Tombstones Slice 4: Daemon and Client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the replication surface (`get_raw`, `list_raw`, `put_raw`, `purge`) over both daemon transports with ADR 0015 auth, stamp the deleter's identity on tombstones through `delete_as`, make `ServerStore` use all of it for real (with a loud upgrade error and no fallback against an old daemon), run tombstone conformance over the daemon, add `GONZALO_ANCESTOR_CAP`, and fold the tombstone cases into `run_store_conformance`.

**Architecture:** `Service` gains pass-throughs for `get_raw`, `list_raw`, `put_raw`, `purge` and `delete_as`. HTTP gains `GET`/`PUT /v1/raw/records/{ns}/{col}/{id}`, `GET /v1/raw/keys` and `POST /v1/purge/{ns}/{col}/{id}`. gRPC gains `GetRaw`, `ListRaw`, `PutRaw` and `Purge`. Authorization works like this:

- Raw reads need `Access::Read` on the namespace, and read-admin when unscoped, exactly like `/v1/keys`.
- `put_raw` needs `Access::Write`. It restamps the author to a non-admin principal, as consumer `put` does, and keeps the incoming author for an admin (including open mode), because an admin token is the replication credential.
- `purge` needs a full admin, checked through a new `Principal::is_admin`.
- The authenticated delete handlers pass the principal's `Identity` to `delete_as`.

`ServerStore` maps HTTP `404` on the replication routes, and gRPC `Unimplemented`, to one fixed upgrade error, and never touches a consumer route. A raw `get` of an absent key answers `200 {"record": null}`, so `404` only ever means "route missing". A put whose `expected` names a revision the store doesn't hold (`CoreError::NotFound`) crosses the wire as HTTP `412` / gRPC `FailedPrecondition` and comes back as `CoreError::NotFound`.

**Tech Stack:** Rust 2024 (MSRV 1.95), axum 0.8.9, tonic 0.12.3 + prost, reqwest 0.12, wiremock 0.6.5, tokio, serde_json.

**Spec:** `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` (§3.1, §3.6, §3.9, §4.1, §6.5). Overview and shared contract: `docs/superpowers/plans/2026-09-13-tombstones-00-overview.md`, as amended by the reconciled contract changes below. Prior slices: `2026-09-13-tombstones-01-core-model-and-trait.md`, `-02-fs-and-git-stores.md`, `-03-s3-store.md`. Precedent for adding routes, RPCs, auth and client methods: `docs/superpowers/plans/2026-07-14-blobstore-over-daemon.md`.

## Global Constraints

- Verification gate before every push and PR, matching CI exactly, run as bare commands, one per line (never joined with `&&`):
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo build --workspace --all-targets --all-features`
  - `cargo test --workspace --all-features`
- Always open a PR and merge it after CI is green. Never push to `main`.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`. PR bodies end with `https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.
- PR body says `Part of #203` (not `Closes`).
- `get_raw`, `list_raw`, `put_raw` and `purge` are required trait methods. **Never** fall back from a replication call to a consumer route or RPC.
- Upgrade error text, verbatim: `daemon predates replication reads (gonzalo#203); upgrade gonzalod`.
- Default ancestor cap: `32` (`gonzalo_core::DEFAULT_ANCESTOR_CAP`). A cap of `0` is rejected at construction.
- Record literals in tests carry the slice-1 fields `ancestors: Vec::new(), deleted_at: None`.
- Workspace lints: `unsafe_code = "forbid"` (gonzalo-server and gonzalo-proto use a local `deny`), clippy `all = warn`, promoted to errors by `-D warnings`. gRPC helpers returning `Result<_, Status>` carry `#[allow(clippy::result_large_err)]`, as the neighbours do.
- Memory is tight on dev machines: run one cargo command at a time, and scope test runs with `-p` until the final gate.

## Reconciled contract this slice consumes (supersedes the overview where they differ)

```rust
// gonzalo-core Store trait (slice 1, reconciled)
async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>>;
async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>>;
/// Replication write: never re-stamps. See `plan_put_raw`.
async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>;
async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult>;
/// Required. `author`, when set, becomes the tombstone's `meta.author`.
async fn delete_as(&self, key: &RecordKey, expected: Option<Revision>, author: Option<Identity>) -> Result<DeleteResult>;
/// Provided: `self.delete_as(key, expected, None)`.
async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult>;

// gonzalo-core/src/tombstone.rs
pub fn tombstone_of(current: &Record, now_ms: i64, cap: usize, author: Option<&Identity>) -> Record;
pub fn plan_delete(current: Option<&Record>, expected: Option<Revision>, now_ms: i64, cap: usize, author: Option<&Identity>) -> DeletePlan;
pub fn plan_put_raw(current: Option<&Record>, record: Record, expected: Option<Revision>, cap: usize) -> PutPlan;
```

- `plan_put_raw`: `None`/`None` → `Write`; `None`/`Some` → `NotFound`; `Some(c)`/`Some(c.revision)` → `Write` verbatim; `Some(c)`/`None` or other → `Conflict { current: c }` (`c` may be a tombstone).
- `plan_put` over a tombstone: `expected == None` → recreation; `Some(anything)` → `NotFound`.
- A conformance factory returns a fresh, empty store on every call.

## Preconditions (check before Task 1)

This slice depends on **slices 1, 2 and 3**: the core contract, `FsStore`/`GitStore`, and `S3Store` (its `with_ancestor_cap` is used by `gonzalod`, and its test file holds a default-cap conformance call that Task 7 removes).

```bash
git switch main
git pull --ff-only
git switch -c feat/203-tombstones-04-daemon-client
rg -n "async fn get_raw|async fn list_raw|async fn put_raw|async fn purge|async fn delete_as" crates/gonzalo-core/src/store.rs
rg -n "pub fn with_ancestor_cap" crates/gonzalo-store-fs/src/lib.rs crates/gonzalo-store-git/src/lib.rs crates/gonzalo-store-s3/src/lib.rs
rg -n "pub async fn run_tombstone_conformance" crates/gonzalo-core/src/conformance.rs
rg -n "pub fn tombstone_hash|pub const DEFAULT_ANCESTOR_CAP|pub fn validate_ancestor_cap|pub fn plan_put_raw" crates/gonzalo-core/src
rg -n "fn (fs|git)_store_passes_tombstone_conformance_default_cap|fn s3_store_passes_tombstone_conformance_at_default_cap" crates
```

Expected:

- five trait-method hits;
- three `with_ancestor_cap` hits;
- one conformance hit;
- four tombstone-module hits;
- three default-cap test functions.

If any are missing, stop: an earlier slice has not merged.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/gonzalo-server/src/auth.rs` | modify | `Principal::is_admin` |
| `crates/gonzalo-proto/src/http.rs` | modify | `RawRecordBody`, `PurgeBody` wire DTOs |
| `crates/gonzalo-proto/proto/gonzalo.proto` | modify | `GetRaw`, `ListRaw`, `PutRaw`, `Purge` RPCs; `PurgeRequest`/`PurgeResponse` |
| `crates/gonzalo-proto/src/lib.rs` | modify | re-export `PurgeRequest`, `PurgeResponse` |
| `crates/gonzalo-server/src/service.rs` | modify | `get_raw`/`list_raw`/`put_raw`/`purge`/`delete_as` pass-throughs (replaces `delete`) + test |
| `crates/gonzalo-server/src/http.rs` | modify | four raw/purge routes, delete stamping, NotFound→412, helpers, DownStore, tests |
| `crates/gonzalo-server/src/grpc.rs` | modify | four RPC handlers, delete stamping, NotFound→FailedPrecondition, helpers, DownStore, tests |
| `crates/gonzalo-store-server/src/lib.rs` | modify | real replication methods on both transports, `delete_as`, upgrade-error and NotFound mapping, tests |
| `crates/gonzalo-store-server/Cargo.toml` | modify | dev-deps `wiremock`, `tokio-stream`, tokio `net` |
| `crates/gonzalo-integration-tests/tests/server_store_conformance.rs` | modify | fresh daemon per factory call; tombstone conformance over both transports |
| `crates/gonzalo-server/src/config.rs` | modify | pure `ancestor_cap_from_env(get)` + tests |
| `crates/gonzalo-server/src/lib.rs` | modify | export `ancestor_cap_from_env` |
| `crates/gonzalo-server/src/bin/gonzalod.rs` | modify | read `GONZALO_ANCESTOR_CAP`, apply to fs and s3 |
| `crates/gonzalo-core/src/conformance.rs` | modify | `run_store_conformance` calls `run_tombstone_conformance(&factory, DEFAULT_ANCESTOR_CAP)` |
| `crates/gonzalo-store-{fs,git}/tests/conformance.rs`, `crates/gonzalo-store-s3/tests/integration.rs` | modify | drop the default-cap tombstone test, keep small-cap |

## Design decisions this plan fixes (read before any task)

1. **Admin is not a role in the code.** It is a `Principal` whose `read` and `write` lists both contain `"*"` (`auth.rs:43-46`, `Principal::admin`). No helper exists today. The unscoped `/v1/keys` check is `principal.allows(Access::Read, "*")` (`http.rs:215-219`, `grpc.rs:192-198`), which is read-admin only. This plan keeps that exact check for unscoped `list_raw` and adds `Principal::is_admin()` (read **and** write on `"*"`) for `purge`. `Principal::open()` is an admin, so open-mode daemons, and the conformance suite over them, can purge.
2. **Raw `get` over HTTP never answers `404` for absence.** Consumer `GET /v1/records/...` uses `404` for "absent". If the raw route did the same, the client could not tell "absent" from "old daemon without the route". The raw route answers `200` with `RawRecordBody { record: Option<Record> }`.
3. **gRPC `GetRaw`/`ListRaw`/`PutRaw` reuse `GetRequest`/`GetResponse`, `ListRequest`/`ListResponse` and `PutRequest`/`PutResponse`.** The shapes are identical, and the graph RPCs already share `GraphQueryRequest`. Only `Purge` gets new messages, because its `expected_json` is a required `Revision`. HTTP `PUT /v1/raw/records/...` reuses `PutBody`/`PutOutcome`.
4. **Purge authorizes before it parses.** HTTP takes the body as `Bytes` and deserializes after the admin check. gRPC calls `authorize_admin` before `serde_json::from_slice`. This is the #146 rule already applied at `grpc.rs:128-138`.
5. **Delete stamps the deleter.** Both delete handlers call `Service::delete_as(key, expected, author)`, where `author = principal.is_authenticated().then(|| Identity::new(principal.name()))`. Open mode passes `None`, and the tombstone keeps the prior author, just as open-mode `put` leaves the author untouched (`http.rs:161-165`). `ServerStore::delete_as` ignores its `author` argument, because the daemon stamps from the bearer token, which a client cannot forge.
6. **`put_raw` keeps authorship unforgeable (ADR 0015).** Authorization is `Access::Write` on the record's namespace, with the same URL-path/body-key agreement check as consumer `put` (#158). The author rule is:
   - Principal is **not** an admin (`!principal.is_admin()`): `record.meta.author` is restamped to the principal's identity, exactly as consumer `put` does. A namespace writer cannot forge authorship through the raw route.
   - Principal **is** an admin: the replicated record's author is kept. **An admin token is the replication credential**: daemon-to-daemon and operator replication run with one, and replication must carry the original writer.
   - Open mode (`Auth::Disabled`) yields `Principal::open()`, which is an admin, so the author is kept. This matches open mode being a transparent store.

   A non-admin principal only exists when auth is enabled, so the single check `!principal.is_admin()` implements "auth enabled and not admin".
7. **`CoreError::NotFound` from a put crosses the wire.** `plan_put` (tombstone + `Some`) and `plan_put_raw` (absent + `Some`) return `Err(CoreError::NotFound(key))`, and conformance asserts `Err(CoreError::NotFound(_))` (slice 1, `put_some_over_tombstone_is_not_found`). Today every handler error is an opaque `500`/`Internal`, so the client would see `Backend`. Put handlers map `NotFound` to HTTP **`412 Precondition Failed`** (not `404`, which on raw routes means "old daemon") and to gRPC **`FailedPrecondition`** (not `NotFound`, to match HTTP). The client maps both back to `CoreError::NotFound(record.key)`.

---

### Task 1: `Principal::is_admin`

**Files:**
- Modify: `crates/gonzalo-server/src/auth.rs:58-75` (add method after `is_authenticated`), tests module `auth.rs:147-244`

**Interfaces:**
- Consumes: nothing new.
- Produces: `impl Principal { pub fn is_admin(&self) -> bool }`: true iff `read` and `write` both contain `"*"`.

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `crates/gonzalo-server/src/auth.rs` (after `disabled_authenticates_everything_as_admin`):

```rust
    #[test]
    fn is_admin_needs_wildcard_read_and_write() {
        assert!(Principal::admin("root").is_admin());
        // Open mode's implicit identity is an admin (it can purge).
        assert!(Principal::open().is_admin());
        // Wildcard on only one side is not an admin.
        assert!(!Principal::new("r", vec!["*".into()], vec![]).is_admin());
        assert!(!Principal::new("w", vec![], vec!["*".into()]).is_admin());
        // Scoped principals are never admins.
        assert!(
            !Principal::new("s", vec!["memory".into()], vec!["memory".into()]).is_admin()
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p gonzalo-server --lib auth::tests::is_admin_needs_wildcard_read_and_write`
Expected: compile error `no method named `is_admin` found for struct `Principal``.

- [ ] **Step 3: Write minimal implementation**

In `crates/gonzalo-server/src/auth.rs`, insert after `is_authenticated` (currently ending at line 65):

```rust
    /// Whether this principal is an admin: `read` **and** `write` on every
    /// namespace (`"*"` in both lists). Operations whose damage is not confined
    /// to one namespace's readers and writers require it, such as `purge`
    /// (gonzalo#203): purging a tombstone early resurrects the record later, on
    /// another machine. Open mode's implicit principal is an admin.
    pub fn is_admin(&self) -> bool {
        self.read.iter().any(|s| s == "*") && self.write.iter().any(|s| s == "*")
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p gonzalo-server --lib auth::tests`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-server/src/auth.rs
git commit -m "feat(server): Principal::is_admin for admin-only operations (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 2: Service pass-throughs and HTTP replication routes

**Files:**
- Modify: `crates/gonzalo-proto/src/http.rs` (append DTOs after `DeleteOutcome`, line 37)
- Modify: `crates/gonzalo-server/src/service.rs:7-10` (imports), `:113-127` (`put`/`list`/`delete` block), tests module
- Modify: `crates/gonzalo-server/src/http.rs:14-15` (imports), `:41-46` (routes), `:74-84` (helpers), `:139-202` (`put_record`, `delete_record`), new handlers after `list_keys` (`:228`), test `DownStore` (`:501-522` plus slice-1 interim methods), new tests
- Test: `crates/gonzalo-server/src/service.rs` and `crates/gonzalo-server/src/http.rs` inline test modules

**Interfaces:**
- Consumes: reconciled `Store` trait (`get_raw`, `list_raw`, `put_raw`, `purge`, `delete_as`); `gonzalo_core::tombstone_hash()`, `RecordKind::Tombstone`; `FsStore` writing tombstones; `Principal::is_admin` (Task 1).
- Produces:
  - `gonzalo_proto::http::RawRecordBody { pub record: Option<Record> }`
  - `gonzalo_proto::http::PurgeBody { pub expected: Revision }`
  - `Service::get_raw(&self, key: &RecordKey) -> Result<Option<Record>>`
  - `Service::list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>>`
  - `Service::put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>`
  - `Service::purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult>`
  - `Service::delete_as(&self, key: &RecordKey, expected: Option<Revision>, author: Option<Identity>) -> Result<DeleteResult>` (replaces `Service::delete`; its only callers were the two transports)
  - HTTP:
    - `GET /v1/raw/records/{ns}/{col}/{id}` → `200 RawRecordBody`.
    - `PUT /v1/raw/records/{ns}/{col}/{id}` with `PutBody` → `200`/`409` `PutOutcome`, `412` NotFound, `400` path/body mismatch, `403`.
    - `GET /v1/raw/keys?namespace=&collection=` → `200 Vec<RecordKey>`.
    - `POST /v1/purge/{ns}/{col}/{id}` with `PurgeBody` → `200`/`409` `DeleteOutcome`, `400` bad body, `403` non-admin.
    - Consumer `PUT /v1/records/...` now answers `412` for `CoreError::NotFound` instead of `500`.

- [ ] **Step 1: Write the failing Service test**

Append inside `mod tests` in `crates/gonzalo-server/src/service.rs`:

```rust
    fn live_record(key: RecordKey) -> Record {
        let body = gonzalo_core::Body::Inline(b"{\"v\":1}".to_vec());
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
            ancestors: Vec::new(),
            deleted_at: None,
            key,
        }
    }

    #[tokio::test]
    async fn replication_methods_delegate_to_the_store() {
        let fs = fresh_fs();
        let svc = Service::new(fs.clone(), fs);
        let key = RecordKey::new("ns", "col", "gone");
        let prefix = KeyPrefix {
            namespace: Some("ns".into()),
            collection: None,
        };

        assert!(matches!(
            svc.put(live_record(key.clone()), None).await.unwrap(),
            PutResult::Committed(_)
        ));
        assert_eq!(
            svc.delete_as(&key, None, Some(Identity::new("deleter")))
                .await
                .unwrap(),
            DeleteResult::Deleted
        );

        // Consumer surface hides the tombstone; raw surface shows it, stamped.
        assert_eq!(svc.get(&key).await.unwrap(), None);
        assert!(!svc.list(&prefix).await.unwrap().contains(&key));
        let tomb = svc.get_raw(&key).await.unwrap().expect("tombstone");
        assert_eq!(tomb.kind, RecordKind::Tombstone);
        assert_eq!(tomb.meta.author, Identity::new("deleter"));
        assert!(svc.list_raw(&prefix).await.unwrap().contains(&key));

        // Purge physically removes it.
        assert_eq!(
            svc.purge(&key, tomb.revision.clone()).await.unwrap(),
            DeleteResult::Deleted
        );
        assert_eq!(svc.get_raw(&key).await.unwrap(), None);

        // put_raw writes verbatim, author included.
        let mut replica = live_record(key.clone());
        replica.meta.author = Identity::new("origin");
        let rev = replica.revision.clone();
        assert_eq!(
            svc.put_raw(replica, None).await.unwrap(),
            PutResult::Committed(rev)
        );
        assert_eq!(
            svc.get_raw(&key).await.unwrap().unwrap().meta.author,
            Identity::new("origin")
        );
    }
```

- [ ] **Step 2: Write the failing HTTP tests**

In `crates/gonzalo-server/src/http.rs`, **replace** the whole test `impl Store for DownStore` (lines 501-522 plus the interim methods slices 1 added) with:

```rust
    /// A store whose every operation fails — models an unreachable backend.
    struct DownStore;

    fn down() -> CoreError {
        CoreError::Backend("store unreachable".into())
    }

    #[async_trait::async_trait]
    impl Store for DownStore {
        async fn get(&self, _key: &RecordKey) -> CoreResult<Option<Record>> {
            Err(down())
        }
        async fn put(&self, _record: Record, _expected: Option<Revision>) -> CoreResult<PutResult> {
            Err(down())
        }
        async fn list(&self, _prefix: &KeyPrefix) -> CoreResult<Vec<RecordKey>> {
            Err(down())
        }
        async fn delete_as(
            &self,
            _key: &RecordKey,
            _expected: Option<Revision>,
            _author: Option<Identity>,
        ) -> CoreResult<DeleteResult> {
            Err(down())
        }
        async fn get_raw(&self, _key: &RecordKey) -> CoreResult<Option<Record>> {
            Err(down())
        }
        async fn list_raw(&self, _prefix: &KeyPrefix) -> CoreResult<Vec<RecordKey>> {
            Err(down())
        }
        async fn put_raw(
            &self,
            _record: Record,
            _expected: Option<Revision>,
        ) -> CoreResult<PutResult> {
            Err(down())
        }
        async fn purge(&self, _key: &RecordKey, _expected: Revision) -> CoreResult<DeleteResult> {
            Err(down())
        }
    }
```

Then append at the end of `mod tests` in `http.rs`:

```rust
    // --- replication surface: raw reads/writes, purge, delete stamping (#203) ---

    /// `reader` reads `memory` only; `writer` reads and writes `memory`;
    /// `admin` is `*`/`*`.
    fn tomb_auth() -> Arc<Auth> {
        Arc::new(Auth::Enabled(std::collections::HashMap::from([
            (
                "rtok".to_string(),
                Principal::new("reader", vec!["memory".into()], vec![]),
            ),
            (
                "wtok".to_string(),
                Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
            ),
            ("atok".to_string(), Principal::admin("admin")),
        ])))
    }

    const LIVE: &str = "/v1/records/memory/col/x";
    const RAW: &str = "/v1/raw/records/memory/col/x";
    const PURGE: &str = "/v1/purge/memory/col/x";

    /// A record at memory/col/x with `author` and `revision`.
    fn record_at(author: &str, revision: Revision) -> Record {
        Record {
            revision,
            parent: None,
            body: gonzalo_core::Body::Inline(b"{}".to_vec()),
            kind: gonzalo_core::RecordKind::MemoryTier,
            meta: gonzalo_core::Meta {
                author: gonzalo_core::Identity::new(author),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: std::collections::BTreeMap::new(),
            },
            links: Vec::new(),
            ancestors: Vec::new(),
            deleted_at: None,
            key: RecordKey::new("memory", "col", "x"),
        }
    }

    fn put_body_with(record: Record, expected: Option<Revision>) -> Vec<u8> {
        serde_json::to_vec(&PutBody { record, expected }).unwrap()
    }

    fn delete_body() -> Vec<u8> {
        serde_json::to_vec(&DeleteBody::default()).unwrap()
    }

    fn purge_body(expected: &Revision) -> Vec<u8> {
        serde_json::to_vec(&PurgeBody {
            expected: expected.clone(),
        })
        .unwrap()
    }

    /// PUT a live record at memory/col/x with `put_token`, then DELETE it with
    /// `delete_token`, leaving a tombstone. Tokens are `None` in open mode.
    async fn seed_tombstone(
        svc: &Service,
        auth: &Arc<Auth>,
        put_token: Option<&str>,
        delete_token: Option<&str>,
    ) {
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            LIVE,
            put_token,
            Some(put_body("memory", "client")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "DELETE",
            LIVE,
            delete_token,
            Some(delete_body()),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    async fn raw_get(
        svc: &Service,
        auth: &Arc<Auth>,
        path: &str,
        token: Option<&str>,
    ) -> (StatusCode, Option<Record>) {
        let (s, body) = call(svc.clone(), auth.clone(), "GET", path, token, None).await;
        let record = if s == StatusCode::OK {
            serde_json::from_slice::<RawRecordBody>(&body)
                .unwrap()
                .record
        } else {
            None
        };
        (s, record)
    }

    async fn keys(svc: &Service, auth: &Arc<Auth>, path: &str, token: &str) -> Vec<RecordKey> {
        let (s, body) = call(svc.clone(), auth.clone(), "GET", path, Some(token), None).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn delete_over_the_wire_leaves_a_tombstone_for_raw_reads() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let key = RecordKey::new("memory", "col", "x");

        // Consumer surface: gone.
        let (s, _) = call(svc.clone(), auth.clone(), "GET", LIVE, Some("rtok"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(
            !keys(&svc, &auth, "/v1/keys?namespace=memory", "rtok")
                .await
                .contains(&key)
        );

        // Raw surface: a reader sees the tombstone.
        let (s, tomb) = raw_get(&svc, &auth, RAW, Some("rtok")).await;
        assert_eq!(s, StatusCode::OK);
        let tomb = tomb.expect("tombstone");
        assert_eq!(tomb.kind, gonzalo_core::RecordKind::Tombstone);
        assert_eq!(tomb.revision.hash, gonzalo_core::tombstone_hash());
        assert!(tomb.deleted_at.is_some());
        assert!(
            keys(&svc, &auth, "/v1/raw/keys?namespace=memory", "rtok")
                .await
                .contains(&key)
        );
    }

    #[tokio::test]
    async fn delete_stamps_the_deleter_on_the_tombstone() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        // `writer` wrote the live record; `admin` deletes it.
        seed_tombstone(&svc, &auth, Some("wtok"), Some("atok")).await;
        let (_, tomb) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        assert_eq!(
            tomb.expect("tombstone").meta.author,
            gonzalo_core::Identity::new("admin")
        );
    }

    #[tokio::test]
    async fn open_mode_delete_keeps_the_prior_author() {
        // Open mode has no identity to stamp (ADR 0015), for delete as for put.
        let (svc, _d) = fs_service();
        let auth = open();
        seed_tombstone(&svc, &auth, None, None).await;
        let (_, tomb) = raw_get(&svc, &auth, RAW, None).await;
        assert_eq!(
            tomb.expect("tombstone").meta.author,
            gonzalo_core::Identity::new("client")
        );
    }

    #[tokio::test]
    async fn raw_reads_need_read_scope_and_absence_is_200_null() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();

        // In scope, absent: 200 with a null record, never 404 (404 is reserved
        // for "this daemon has no raw route").
        let (s, rec) = raw_get(&svc, &auth, "/v1/raw/records/memory/col/absent", Some("rtok")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(rec, None);

        // Out of scope.
        let (s, _) = raw_get(&svc, &auth, "/v1/raw/records/secrets/col/x", Some("rtok")).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "GET",
            "/v1/raw/keys?namespace=secrets",
            Some("rtok"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // No token.
        let (s, _) = call(svc, auth, "GET", RAW, None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unscoped_list_raw_requires_admin() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        for token in ["rtok", "wtok"] {
            let (s, _) = call(
                svc.clone(),
                auth.clone(),
                "GET",
                "/v1/raw/keys",
                Some(token),
                None,
            )
            .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{token}");
        }
        let (s, _) = call(svc, auth, "GET", "/v1/raw/keys", Some("atok"), None).await;
        assert_eq!(s, StatusCode::OK);
    }

    /// `put_raw` a record claiming author `"origin"` with `token`, assert it
    /// committed at the incoming revision, and return the stored author.
    async fn put_raw_author(
        svc: &Service,
        auth: &Arc<Auth>,
        token: Option<&str>,
    ) -> gonzalo_core::Identity {
        let replica = record_at("origin", Revision::initial(b"{}"));
        let (s, body) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            token,
            Some(put_body_with(replica.clone(), None)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(matches!(
            serde_json::from_slice::<PutOutcome>(&body).unwrap(),
            PutOutcome::Committed { revision } if revision == replica.revision
        ));
        let (_, stored) = raw_get(svc, auth, RAW, token).await;
        stored.expect("stored").meta.author
    }

    #[tokio::test]
    async fn put_raw_needs_write_scope_and_matching_path() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        let replica = record_at("origin", Revision::initial(b"{}"));

        // A reader may not write.
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            Some("rtok"),
            Some(put_body_with(replica.clone(), None)),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // Path and body key must agree (#158).
        let (s, _) = call(
            svc,
            auth,
            "PUT",
            "/v1/raw/records/memory/col/y",
            Some("wtok"),
            Some(put_body_with(replica, None)),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_raw_by_scoped_writer_is_stamped_with_the_writer() {
        // A non-admin cannot forge authorship through the raw route (ADR 0015).
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        assert_eq!(
            put_raw_author(&svc, &auth, Some("wtok")).await,
            gonzalo_core::Identity::new("writer")
        );
    }

    #[tokio::test]
    async fn put_raw_by_admin_keeps_the_incoming_author() {
        // An admin token is the replication credential: the original writer
        // survives replication.
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        assert_eq!(
            put_raw_author(&svc, &auth, Some("atok")).await,
            gonzalo_core::Identity::new("origin")
        );
    }

    #[tokio::test]
    async fn put_raw_in_open_mode_keeps_the_incoming_author() {
        // Open mode's implicit principal is an admin.
        let (svc, _d) = fs_service();
        let auth = open();
        assert_eq!(
            put_raw_author(&svc, &auth, None).await,
            gonzalo_core::Identity::new("origin")
        );
    }

    #[tokio::test]
    async fn put_raw_overwrites_a_tombstone_verbatim_and_none_conflicts() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let (_, tomb) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        let tomb = tomb.expect("tombstone");

        // expected = None over a tombstone is a conflict for put_raw, and the
        // conflict may carry the tombstone.
        let peer = record_at("peer", Revision::initial(b"peer"));
        let (s, body) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            Some("wtok"),
            Some(put_body_with(peer.clone(), None)),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(matches!(
            serde_json::from_slice::<PutOutcome>(&body).unwrap(),
            PutOutcome::Conflict { conflict } if conflict.current.revision == tomb.revision
        ));

        // expected = the tombstone's revision writes the record unchanged.
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            Some("wtok"),
            Some(put_body_with(peer.clone(), Some(tomb.revision.clone()))),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, stored) = raw_get(&svc, &auth, RAW, Some("rtok")).await;
        let stored = stored.expect("stored");
        assert_eq!(stored.revision, peer.revision);
        assert_eq!(stored.kind, gonzalo_core::RecordKind::MemoryTier);
    }

    #[tokio::test]
    async fn put_not_found_is_412_on_both_put_routes() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        let never = Revision::initial(b"never current");

        // put_raw with Some over an absent key → NotFound → 412.
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            Some("wtok"),
            Some(put_body_with(record_at("w", Revision::initial(b"{}")), Some(never.clone()))),
        )
        .await;
        assert_eq!(s, StatusCode::PRECONDITION_FAILED);

        // Consumer put with Some over a tombstone → NotFound → 412.
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let (s, _) = call(
            svc,
            auth,
            "PUT",
            LIVE,
            Some("wtok"),
            Some(put_body_with(record_at("w", Revision::initial(b"{}")), Some(never))),
        )
        .await;
        assert_eq!(s, StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn purge_requires_admin() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let (_, tomb) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        let rev = tomb.expect("tombstone").revision;

        for token in ["rtok", "wtok"] {
            let (s, _) = call(
                svc.clone(),
                auth.clone(),
                "POST",
                PURGE,
                Some(token),
                Some(purge_body(&rev)),
            )
            .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{token}");
        }

        let (s, body) = call(
            svc.clone(),
            auth.clone(),
            "POST",
            PURGE,
            Some("atok"),
            Some(purge_body(&rev)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(matches!(
            serde_json::from_slice::<DeleteOutcome>(&body).unwrap(),
            DeleteOutcome::Deleted
        ));
        let (s, rec) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(rec, None);
    }

    #[tokio::test]
    async fn purge_with_stale_expected_is_409() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let (s, body) = call(
            svc,
            auth,
            "POST",
            PURGE,
            Some("atok"),
            Some(purge_body(&Revision::initial(b"not the tombstone"))),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(matches!(
            serde_json::from_slice::<DeleteOutcome>(&body).unwrap(),
            DeleteOutcome::Conflict { .. }
        ));
    }

    #[tokio::test]
    async fn purge_authorizes_before_parsing_the_body() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "POST",
            PURGE,
            Some("wtok"),
            Some(b"not json".to_vec()),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(svc, auth, "POST", PURGE, Some("atok"), Some(b"not json".to_vec())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn raw_backend_error_is_opaque() {
        let dir = TempDir::new().unwrap();
        let blobs = Arc::new(FsStore::new(dir.path()));
        let svc = Service::new(Arc::new(DownStore), blobs);
        let (s, body) = call(svc, scoped(), "GET", RAW, Some("wtok"), None).await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(String::from_utf8(body).unwrap(), "internal error");
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p gonzalo-server --lib`
Expected: compile errors: `cannot find struct `RawRecordBody``, `cannot find struct `PurgeBody``, `no method named `get_raw` found for struct `Service``, `no method named `delete_as` found for struct `Service``.

- [ ] **Step 4: Add the wire DTOs**

Append to `crates/gonzalo-proto/src/http.rs`:

```rust
/// Response of `GET /v1/raw/records/{ns}/{col}/{id}` (gonzalo#203). Absence is
/// `200 {"record": null}`, never `404`: a `404` from this route means the
/// daemon predates replication reads, and the client must tell the two apart
/// without guessing. `PUT` on the same path takes a [`PutBody`] and answers a
/// [`PutOutcome`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawRecordBody {
    pub record: Option<Record>,
}

/// Body of `POST /v1/purge/{ns}/{col}/{id}` (gonzalo#203). The key is addressed
/// by the URL path. `expected` is required, because purge is always conditional
/// on the current revision. The response is a [`DeleteOutcome`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeBody {
    pub expected: Revision,
}
```

- [ ] **Step 5: Add the Service pass-throughs**

In `crates/gonzalo-server/src/service.rs`, replace the `gonzalo_core` import (lines 7-10):

```rust
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, Manifest, PutResult,
    Record, RecordKey, Result, Revision, Store,
};
```

Replace `delete` (lines 121-127) with:

```rust
    /// Delete `key` by writing a tombstone. `author`, when set, becomes the
    /// tombstone's `meta.author`: transports pass the authenticated principal
    /// and open mode passes `None` (ADR 0015).
    pub async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        self.store.delete_as(key, expected, author).await
    }

    // --- Replication surface (gonzalo#203): tombstones visible ---

    /// Like [`get`](Self::get), but returns tombstones. Replication only.
    pub async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.store.get_raw(key).await
    }

    /// Like [`list`](Self::list), but includes tombstoned keys. Replication only.
    pub async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.store.list_raw(prefix).await
    }

    /// Replication write: stores `record` verbatim (never re-stamped).
    pub async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        self.store.put_raw(record, expected).await
    }

    /// Physically remove the record at `key` iff its current revision is
    /// `expected`. The only physical removal in the system.
    pub async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        self.store.purge(key, expected).await
    }
```

- [ ] **Step 6: Add the HTTP routes and handlers**

In `crates/gonzalo-server/src/http.rs`:

Replace the imports at lines 14-15:

```rust
use gonzalo_core::{
    ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, PutResult, RecordKey,
};
use gonzalo_proto::http::{
    DeleteBody, DeleteOutcome, PurgeBody, PutBody, PutOutcome, RawRecordBody,
};
```

Replace the record routes at lines 41-45:

```rust
        .route(
            "/v1/records/{ns}/{col}/{id}",
            get(get_record).put(put_record).delete(delete_record),
        )
        .route("/v1/keys", get(list_keys))
        // Replication surface (gonzalo#203): tombstones visible, purge is admin.
        .route(
            "/v1/raw/records/{ns}/{col}/{id}",
            get(get_raw_record).put(put_raw_record),
        )
        .route("/v1/raw/keys", get(list_raw_keys))
        .route(
            "/v1/purge/{ns}/{col}/{id}",
            axum::routing::post(purge_record),
        )
```

After `forbidden` (lines 74-84) add:

```rust
/// `403` when an operation requires an admin (`read` and `write` on `"*"`).
fn forbidden_admin(principal: &Principal, operation: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        format!(
            "principal {:?} is not an admin; {operation} requires admin",
            principal.name()
        ),
    )
        .into_response()
}

/// `400` when the URL path and the body's record key disagree (#158), else
/// `None`. Shared by `PUT /v1/records/...` and `PUT /v1/raw/records/...`.
fn path_key_mismatch(path: &(String, String, String), key: &RecordKey) -> Option<Response> {
    let (ns, col, id) = path;
    (*ns != key.namespace || *col != key.collection || *id != key.id).then(|| {
        (
            StatusCode::BAD_REQUEST,
            "URL path does not match record key",
        )
            .into_response()
    })
}

/// `200` + `Committed`, `409` + `Conflict`, `412` when `expected` names a
/// revision the store does not hold (`CoreError::NotFound`), or an opaque
/// `500`. `412` rather than `404`: on the raw routes a `404` means "old daemon".
/// Shared by both put routes.
fn put_outcome_response(result: gonzalo_core::Result<PutResult>) -> Response {
    match result {
        Ok(PutResult::Committed(revision)) => {
            (StatusCode::OK, Json(PutOutcome::Committed { revision })).into_response()
        }
        Ok(PutResult::Conflict(conflict)) => (
            StatusCode::CONFLICT,
            Json(PutOutcome::Conflict { conflict }),
        )
            .into_response(),
        Err(CoreError::NotFound(key)) => (
            StatusCode::PRECONDITION_FAILED,
            format!("record not found: {key}"),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

/// `200` + `Deleted`, `409` + `Conflict`, or an opaque `500`. Shared by
/// `DELETE /v1/records/...` and `POST /v1/purge/...`.
fn delete_outcome_response(result: gonzalo_core::Result<DeleteResult>) -> Response {
    match result {
        Ok(DeleteResult::Deleted) => (StatusCode::OK, Json(DeleteOutcome::Deleted)).into_response(),
        Ok(DeleteResult::Conflict(conflict)) => (
            StatusCode::CONFLICT,
            Json(DeleteOutcome::Conflict { conflict }),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}
```

Replace `put_record` and `delete_record` (lines 139-202) with:

```rust
async fn put_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<(String, String, String)>,
    Json(mut body): Json<PutBody>,
) -> Response {
    // The URL path addresses the record; the body must agree with it. Without
    // this check the path is decorative and authz/write key off the body alone,
    // so a path-based proxy control could be bypassed by a mismatched body
    // (#158). Reject the disagreement with 400 before any authz or write.
    if let Some(bad) = path_key_mismatch(&path, &body.record.key) {
        return bad;
    }
    let ns = &body.record.key.namespace;
    if !principal.allows(Access::Write, ns) {
        return forbidden(&principal, Access::Write, &ns.clone());
    }
    // Stamp the author from the authenticated principal — unforgeable (ADR
    // 0015). Open mode (no auth) leaves the record's author untouched.
    if principal.is_authenticated() {
        body.record.meta.author = Identity::new(principal.name());
    }
    put_outcome_response(svc.put(body.record, body.expected).await)
}

/// The URL path addresses the record; the OCC precondition rides in an optional
/// JSON body. Authorize `Write` on the path's namespace, then delegate — the key
/// is taken from the path, so there is no body-key-vs-path check to make.
///
/// The store writes a tombstone (gonzalo#203) stamped with the authenticated
/// principal as its author, exactly as `put` stamps. Open mode has no identity
/// and passes `None`, so the tombstone keeps the prior author.
async fn delete_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path((ns, col, id)): Path<(String, String, String)>,
    body: Option<Json<DeleteBody>>,
) -> Response {
    if !principal.allows(Access::Write, &ns) {
        return forbidden(&principal, Access::Write, &ns);
    }
    let expected = body.map(|Json(b)| b.expected).unwrap_or_default();
    let author = principal
        .is_authenticated()
        .then(|| Identity::new(principal.name()));
    let key = RecordKey::new(ns, col, id);
    delete_outcome_response(svc.delete_as(&key, expected, author).await)
}
```

After `list_keys` (ends line 228) add:

```rust
/// `GET /v1/raw/records/{ns}/{col}/{id}` — replication read that includes
/// tombstones (gonzalo#203). Always `200` with a [`RawRecordBody`]; absence is
/// `{"record": null}` so `404` unambiguously means "no such route".
async fn get_raw_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path((ns, col, id)): Path<(String, String, String)>,
) -> Response {
    if !principal.allows(Access::Read, &ns) {
        return forbidden(&principal, Access::Read, &ns);
    }
    match svc.get_raw(&RecordKey::new(ns, col, id)).await {
        Ok(record) => (StatusCode::OK, Json(RawRecordBody { record })).into_response(),
        Err(e) => server_error(e),
    }
}

/// `PUT /v1/raw/records/{ns}/{col}/{id}` with a [`PutBody`] — replication write
/// (gonzalo#203). The store writes the record verbatim (revision and
/// tombstones included). Authorization is `Write` on the namespace, with the
/// same path/body agreement check as `put` (#158).
///
/// **Authorship stays unforgeable (ADR 0015).** An admin token is the
/// replication credential: daemon-to-daemon and operator replication run with
/// one, so an admin's raw write keeps the replicated record's original
/// `meta.author`. Any other principal is restamped exactly as `put` restamps,
/// so a namespace writer cannot forge authorship through the raw route. Open
/// mode's implicit principal is an admin, so it keeps the author too.
async fn put_raw_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<(String, String, String)>,
    Json(mut body): Json<PutBody>,
) -> Response {
    if let Some(bad) = path_key_mismatch(&path, &body.record.key) {
        return bad;
    }
    let ns = &body.record.key.namespace;
    if !principal.allows(Access::Write, ns) {
        return forbidden(&principal, Access::Write, &ns.clone());
    }
    if !principal.is_admin() {
        body.record.meta.author = Identity::new(principal.name());
    }
    put_outcome_response(svc.put_raw(body.record, body.expected).await)
}

/// `GET /v1/raw/keys?namespace=&collection=` — replication list that includes
/// tombstoned keys. Same authorization as `/v1/keys`: no namespace spans all
/// namespaces and requires `read` on `"*"`.
async fn list_raw_keys(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> Response {
    let ns = q.namespace.as_deref().unwrap_or("*");
    if !principal.allows(Access::Read, ns) {
        return forbidden(&principal, Access::Read, ns);
    }
    let prefix = KeyPrefix {
        namespace: q.namespace,
        collection: q.collection,
    };
    match svc.list_raw(&prefix).await {
        Ok(keys) => (StatusCode::OK, Json(keys)).into_response(),
        Err(e) => server_error(e),
    }
}

/// `POST /v1/purge/{ns}/{col}/{id}` with a [`PurgeBody`] — physically remove the
/// record iff its revision is `expected` (gonzalo#203). **Admin only**: an early
/// purge is data loss that surfaces later on another machine (spec §5.2). The
/// body is parsed only after the admin check (#146).
async fn purge_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path((ns, col, id)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    if !principal.is_admin() {
        return forbidden_admin(&principal, "purge");
    }
    let PurgeBody { expected } = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid purge body: {e}")).into_response();
        }
    };
    let key = RecordKey::new(ns, col, id);
    delete_outcome_response(svc.purge(&key, expected).await)
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p gonzalo-server --lib`
Expected: compile error in `grpc.rs` only (`no method named `delete` found for struct `Service``). Task 3 fixes this. So that this task can be verified on its own, make a one-line interim edit at `grpc.rs:172-176`: change `.delete(&key, expected)` to `.delete_as(&key, expected, None)`. Then rerun.

Run: `cargo test -p gonzalo-server --lib`
Expected: PASS. New: `service::tests::replication_methods_delegate_to_the_store`, and these `http::tests` (15 new plus one pre-existing):

- `delete_over_the_wire_leaves_a_tombstone_for_raw_reads`
- `delete_stamps_the_deleter_on_the_tombstone`
- `open_mode_delete_keeps_the_prior_author`
- `raw_reads_need_read_scope_and_absence_is_200_null`
- `unscoped_list_raw_requires_admin`
- `put_raw_needs_write_scope_and_matching_path`
- `put_raw_by_scoped_writer_is_stamped_with_the_writer`
- `put_raw_by_admin_keeps_the_incoming_author`
- `put_raw_in_open_mode_keeps_the_incoming_author`
- `put_raw_overwrites_a_tombstone_verbatim_and_none_conflicts`
- `put_not_found_is_412_on_both_put_routes`
- `purge_requires_admin`
- `purge_with_stale_expected_is_409`
- `purge_authorizes_before_parsing_the_body`
- `raw_backend_error_is_opaque`
- the pre-existing `put_rejects_url_path_body_key_mismatch`, still green through `path_key_mismatch`

Every other pre-existing test also passes.

- [ ] **Step 8: Commit**

```bash
git add crates/gonzalo-proto/src/http.rs crates/gonzalo-server/src/service.rs crates/gonzalo-server/src/http.rs crates/gonzalo-server/src/grpc.rs
git commit -m "feat(server): HTTP raw read/write, admin purge, stamped deletes (#203)

GET /v1/raw/records answers 200 {\"record\": null} for absence so a 404
only ever means an old daemon. PUT /v1/raw/records keeps the incoming
author only for admins (the replication credential) and restamps anyone
else, per ADR 0015. Deletes stamp the authenticated principal via
delete_as. Put NotFound is 412 instead of an opaque 500.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 3: gRPC `GetRaw`, `ListRaw`, `PutRaw`, `Purge`

The proto change and the handlers ship together: adding RPCs makes the generated `Gonzalo` trait require the new methods.

**Files:**
- Modify: `crates/gonzalo-proto/proto/gonzalo.proto:8-13` (service), append messages after line 140
- Modify: `crates/gonzalo-proto/src/lib.rs:9-14` (re-exports)
- Modify: `crates/gonzalo-server/src/grpc.rs:6-16` (imports), `:44-86` (add `authorize_admin`), `:108-210` (`get`, `put`, `delete`, `list`), new handlers, helpers before `located_response` (`:377`), test `DownStore` (`:559-595` plus interim methods), new tests
- Test: `crates/gonzalo-server/src/grpc.rs` inline tests

**Interfaces:**
- Consumes: `Service::get_raw`/`list_raw`/`put_raw`/`purge`/`delete_as` (Task 2); `Principal::is_admin` (Task 1).
- Produces:
  - proto RPCs:
    - `rpc GetRaw(GetRequest) returns (GetResponse)`
    - `rpc ListRaw(ListRequest) returns (ListResponse)`
    - `rpc PutRaw(PutRequest) returns (PutResponse)`
    - `rpc Purge(PurgeRequest) returns (PurgeResponse)`
  - proto messages `PurgeRequest { namespace, collection, id, expected_json }` and `PurgeResponse { outcome, payload_json }`.
  - Generated client methods used by Task 4: `GonzaloClient::get_raw`, `::list_raw`, `::put_raw`, `::purge`.
  - `Put` and `PutRaw` map `CoreError::NotFound` to `Status::failed_precondition`.

- [ ] **Step 1: Write the failing tests**

In `crates/gonzalo-server/src/grpc.rs`, **replace** the whole test `impl Store for DownStore` (lines 559-595 plus interim methods) with:

```rust
    /// A store whose every op fails, to force the `internal` (server-error) path.
    struct DownStore;

    fn leaky() -> gonzalo_core::CoreError {
        gonzalo_core::CoreError::Backend("s3://secret-bucket".into())
    }

    #[async_trait::async_trait]
    impl Store for DownStore {
        async fn get(&self, _key: &RecordKey) -> gonzalo_core::Result<Option<Record>> {
            Err(gonzalo_core::CoreError::Backend(
                "/var/lib/gonzalo/graphs/secret.sqlite unreachable".into(),
            ))
        }
        async fn put(
            &self,
            _record: Record,
            _expected: Option<Revision>,
        ) -> gonzalo_core::Result<PutResult> {
            Err(leaky())
        }
        async fn list(
            &self,
            _prefix: &gonzalo_core::KeyPrefix,
        ) -> gonzalo_core::Result<Vec<RecordKey>> {
            Err(leaky())
        }
        async fn delete_as(
            &self,
            _key: &RecordKey,
            _expected: Option<Revision>,
            _author: Option<Identity>,
        ) -> gonzalo_core::Result<DeleteResult> {
            Err(leaky())
        }
        async fn get_raw(&self, _key: &RecordKey) -> gonzalo_core::Result<Option<Record>> {
            Err(leaky())
        }
        async fn list_raw(
            &self,
            _prefix: &gonzalo_core::KeyPrefix,
        ) -> gonzalo_core::Result<Vec<RecordKey>> {
            Err(leaky())
        }
        async fn put_raw(
            &self,
            _record: Record,
            _expected: Option<Revision>,
        ) -> gonzalo_core::Result<PutResult> {
            Err(leaky())
        }
        async fn purge(
            &self,
            _key: &RecordKey,
            _expected: Revision,
        ) -> gonzalo_core::Result<DeleteResult> {
            Err(leaky())
        }
    }
```

Append at the end of `mod tests` in `grpc.rs`:

```rust
    // --- replication RPCs: GetRaw / ListRaw / PutRaw / Purge (gonzalo#203) ---

    /// `reader` reads `memory` only; `writer` reads and writes `memory`; `admin`.
    fn tomb_auth() -> Arc<Auth> {
        Arc::new(Auth::Enabled(HashMap::from([
            (
                "rtok".to_string(),
                Principal::new("reader", vec!["memory".into()], vec![]),
            ),
            (
                "wtok".to_string(),
                Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
            ),
            ("atok".to_string(), Principal::admin("admin")),
        ])))
    }

    fn delete_req(namespace: &str) -> DeleteRequest {
        DeleteRequest {
            namespace: namespace.into(),
            collection: "col".into(),
            id: "x".into(),
            expected_json: serde_json::to_vec(&Option::<Revision>::None).unwrap(),
        }
    }

    fn purge_req(namespace: &str, expected: &Revision) -> PurgeRequest {
        PurgeRequest {
            namespace: namespace.into(),
            collection: "col".into(),
            id: "x".into(),
            expected_json: serde_json::to_vec(expected).unwrap(),
        }
    }

    /// A `PutRequest` for memory/col/x with an explicit author and precondition.
    fn put_req_with(author: &str, payload: &[u8], expected: Option<Revision>) -> PutRequest {
        let body = gonzalo_core::Body::Inline(payload.to_vec());
        let record = Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind: RecordKind::MemoryTier,
            meta: Meta {
                author: Identity::new(author),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            ancestors: Vec::new(),
            deleted_at: None,
            key: RecordKey::new("memory", "col", "x"),
        };
        PutRequest {
            record_json: serde_json::to_vec(&record).unwrap(),
            expected_json: serde_json::to_vec(&expected).unwrap(),
        }
    }

    fn memory_list() -> ListRequest {
        ListRequest {
            namespace: Some("memory".into()),
            collection: None,
        }
    }

    async fn raw_record(adapter: &GrpcAdapter) -> Option<Record> {
        let raw = adapter
            .get_raw(with_token(get_req("memory"), "atok"))
            .await
            .unwrap()
            .into_inner();
        raw.found
            .then(|| serde_json::from_slice(&raw.record_json).unwrap())
    }

    /// Put memory/col/x as `writer`, delete it with `delete_token`; return the
    /// tombstone.
    async fn seed_tombstone(adapter: &GrpcAdapter, delete_token: &str) -> Record {
        adapter
            .put(with_token(put_req("memory", "writer"), "wtok"))
            .await
            .unwrap();
        let del = adapter
            .delete(with_token(delete_req("memory"), delete_token))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(del.outcome, "deleted");
        raw_record(adapter).await.expect("tombstone")
    }

    fn decode_keys(resp: ListResponse) -> Vec<RecordKey> {
        resp.keys_json
            .iter()
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn grpc_delete_leaves_a_stamped_tombstone_for_raw_reads() {
        let adapter = fs_adapter(tomb_auth());
        // `writer` wrote it; `admin` deletes it; the tombstone names the deleter.
        let tomb = seed_tombstone(&adapter, "atok").await;
        assert_eq!(tomb.kind, RecordKind::Tombstone);
        assert_eq!(tomb.revision.hash, gonzalo_core::tombstone_hash());
        assert_eq!(tomb.meta.author, Identity::new("admin"));
        let key = RecordKey::new("memory", "col", "x");

        let got = adapter
            .get(with_token(get_req("memory"), "rtok"))
            .await
            .unwrap()
            .into_inner();
        assert!(!got.found);
        let live = decode_keys(
            adapter
                .list(with_token(memory_list(), "rtok"))
                .await
                .unwrap()
                .into_inner(),
        );
        assert!(!live.contains(&key));

        let raw = adapter
            .get_raw(with_token(get_req("memory"), "rtok"))
            .await
            .unwrap()
            .into_inner();
        assert!(raw.found);
        let raw_keys = decode_keys(
            adapter
                .list_raw(with_token(memory_list(), "rtok"))
                .await
                .unwrap()
                .into_inner(),
        );
        assert!(raw_keys.contains(&key));
    }

    #[tokio::test]
    async fn grpc_raw_reads_are_read_scoped() {
        let adapter = fs_adapter(tomb_auth());
        let err = adapter
            .get_raw(with_token(get_req("secrets"), "rtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let err = adapter
            .get_raw(Request::new(get_req("memory")))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn grpc_unscoped_list_raw_requires_admin() {
        let adapter = fs_adapter(tomb_auth());
        for token in ["rtok", "wtok"] {
            let err = adapter
                .list_raw(with_token(ListRequest::default(), token))
                .await
                .unwrap_err();
            assert_eq!(err.code(), tonic::Code::PermissionDenied, "{token}");
        }
        assert!(
            adapter
                .list_raw(with_token(ListRequest::default(), "atok"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn grpc_put_raw_is_write_scoped() {
        let adapter = fs_adapter(tomb_auth());
        let err = adapter
            .put_raw(with_token(put_req_with("origin", b"{}", None), "rtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn grpc_put_raw_by_scoped_writer_is_stamped_with_the_writer() {
        // A non-admin cannot forge authorship through PutRaw (ADR 0015).
        let adapter = fs_adapter(tomb_auth());
        let resp = adapter
            .put_raw(with_token(put_req_with("origin", b"{}", None), "wtok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.outcome, "committed");
        assert_eq!(
            raw_record(&adapter).await.expect("stored").meta.author,
            Identity::new("writer")
        );
    }

    #[tokio::test]
    async fn grpc_put_raw_by_admin_keeps_the_incoming_author() {
        // An admin token is the replication credential.
        let adapter = fs_adapter(tomb_auth());
        let resp = adapter
            .put_raw(with_token(put_req_with("origin", b"{}", None), "atok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.outcome, "committed");
        assert_eq!(
            raw_record(&adapter).await.expect("stored").meta.author,
            Identity::new("origin")
        );
    }

    #[tokio::test]
    async fn grpc_put_raw_in_open_mode_keeps_the_incoming_author() {
        // Open mode's implicit principal is an admin.
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let adapter = GrpcAdapter::new(Service::new(fs.clone(), fs));
        let resp = adapter
            .put_raw(Request::new(put_req_with("origin", b"{}", None)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.outcome, "committed");
        let raw = adapter
            .get_raw(Request::new(get_req("memory")))
            .await
            .unwrap()
            .into_inner();
        let stored: Record = serde_json::from_slice(&raw.record_json).unwrap();
        assert_eq!(stored.meta.author, Identity::new("origin"));
    }

    #[tokio::test]
    async fn grpc_put_raw_over_tombstone_none_conflicts_some_overwrites() {
        let adapter = fs_adapter(tomb_auth());
        let tomb = seed_tombstone(&adapter, "wtok").await;

        let conflict = adapter
            .put_raw(with_token(put_req_with("peer", b"peer", None), "wtok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(conflict.outcome, "conflict");

        let ok = adapter
            .put_raw(with_token(
                put_req_with("peer", b"peer", Some(tomb.revision.clone())),
                "wtok",
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ok.outcome, "committed");
        let stored = raw_record(&adapter).await.expect("stored");
        assert_eq!(stored.revision, Revision::initial(b"peer"));
    }

    #[tokio::test]
    async fn grpc_put_not_found_is_failed_precondition() {
        let adapter = fs_adapter(tomb_auth());
        let never = Some(Revision::initial(b"never current"));
        let err = adapter
            .put_raw(with_token(put_req_with("w", b"{}", never.clone()), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);

        seed_tombstone(&adapter, "wtok").await;
        let err = adapter
            .put(with_token(put_req_with("w", b"{}", never), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn grpc_purge_requires_admin() {
        let adapter = fs_adapter(tomb_auth());
        let tomb = seed_tombstone(&adapter, "wtok").await;

        for token in ["rtok", "wtok"] {
            let err = adapter
                .purge(with_token(purge_req("memory", &tomb.revision), token))
                .await
                .unwrap_err();
            assert_eq!(err.code(), tonic::Code::PermissionDenied, "{token}");
        }
        let err = adapter
            .purge(Request::new(purge_req("memory", &tomb.revision)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        let ok = adapter
            .purge(with_token(purge_req("memory", &tomb.revision), "atok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ok.outcome, "deleted");
        assert_eq!(raw_record(&adapter).await, None);
    }

    #[tokio::test]
    async fn grpc_purge_stale_expected_is_conflict() {
        let adapter = fs_adapter(tomb_auth());
        seed_tombstone(&adapter, "wtok").await;
        let resp = adapter
            .purge(with_token(
                purge_req("memory", &Revision::initial(b"not the tombstone")),
                "atok",
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.outcome, "conflict");
        let _: gonzalo_core::Conflict = serde_json::from_slice(&resp.payload_json).unwrap();
    }

    #[tokio::test]
    async fn grpc_purge_authorizes_before_parsing() {
        let adapter = fs_adapter(tomb_auth());
        let garbage = PurgeRequest {
            namespace: "memory".into(),
            collection: "col".into(),
            id: "x".into(),
            expected_json: b"not json".to_vec(),
        };
        let err = adapter
            .purge(with_token(garbage.clone(), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let err = adapter
            .purge(with_token(garbage, "atok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn grpc_raw_backend_error_is_opaque() {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let adapter = GrpcAdapter::new(Service::new(Arc::new(DownStore), fs));
        let err = adapter
            .get_raw(Request::new(get_req("any")))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Internal);
        assert_eq!(err.message(), "internal error");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p gonzalo-server --lib grpc::tests`
Expected: compile errors: `cannot find struct `PurgeRequest``, `no method named `get_raw` found for struct `GrpcAdapter``.

- [ ] **Step 3: Extend the proto**

In `crates/gonzalo-proto/proto/gonzalo.proto`, replace lines 8-13 (service head through `TicketSync`) with:

```proto
service Gonzalo {
  rpc Get(GetRequest) returns (GetResponse);
  rpc Put(PutRequest) returns (PutResponse);
  rpc Delete(DeleteRequest) returns (DeleteResponse);
  rpc List(ListRequest) returns (ListResponse);
  // Replication surface (gonzalo#203). Raw reads include tombstones; PutRaw
  // stores a record verbatim (revision never re-stamped; the author is kept
  // only for an admin principal, per ADR 0015); Purge is the only
  // physical removal and requires an admin. GetRaw/ListRaw/PutRaw reuse the
  // Get/List/Put messages: the shapes are identical, only the store call
  // differs.
  rpc GetRaw(GetRequest) returns (GetResponse);
  rpc ListRaw(ListRequest) returns (ListResponse);
  rpc PutRaw(PutRequest) returns (PutResponse);
  rpc Purge(PurgeRequest) returns (PurgeResponse);
  rpc TicketSync(TicketSyncRequest) returns (TicketSyncResponse);
```

Append at the end of the file:

```proto

// --- Replication surface (gonzalo#203) ---
message PurgeRequest {
  string namespace = 1;
  string collection = 2;
  string id = 3;
  // JSON of gonzalo_core::Revision. Required: purge is always conditional.
  bytes expected_json = 4;
}

message PurgeResponse {
  // "deleted" or "conflict".
  string outcome = 1;
  // Empty (deleted) or JSON of Conflict (conflict).
  bytes payload_json = 2;
}
```

In `crates/gonzalo-proto/src/lib.rs`, replace the `pub use v1::{...}` block (lines 9-14) with:

```rust
pub use v1::{
    DeleteRequest, DeleteResponse, GetRequest, GetResponse, GraphLocatedResponse,
    GraphNamesResponse, GraphQueryRequest, ListRequest, ListResponse, PurgeRequest,
    PurgeResponse, PutRequest, PutResponse,
    gonzalo_client::GonzaloClient,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
```

- [ ] **Step 4: Implement the handlers**

In `crates/gonzalo-server/src/grpc.rs`, replace the imports at lines 6-16:

```rust
use gonzalo_core::{
    ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, PutResult, Record, RecordKey,
    Revision,
};
use gonzalo_proto::v1::{
    DeleteBlobRequest, DeleteBlobResponse, DeleteRequest, DeleteResponse, GetBlobRequest,
    GetBlobResponse, GetRequest, GetResponse, GraphLocatedResponse, GraphNamesResponse,
    GraphQueryRequest, ListBlobsRequest, ListBlobsResponse, ListRequest, ListResponse,
    PurgeRequest, PurgeResponse, PutBlobRequest, PutBlobResponse, PutRequest, PutResponse,
    TicketSyncRequest, TicketSyncResponse,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
```

Inside `impl GrpcAdapter`, after `check_access` (ends line 85), add:

```rust
    /// Authenticate the call and require an admin principal (`read` and
    /// `write` on `"*"`). Used by `purge` (gonzalo#203): an early purge is data
    /// loss that surfaces later on another machine, so no namespace scope is
    /// enough. Takes no body, so it runs before any deserialization (#146).
    #[allow(clippy::result_large_err)]
    fn authorize_admin(&self, metadata: &MetadataMap, operation: &str) -> Result<Principal, Status> {
        let principal = self.authenticate(metadata)?;
        if principal.is_admin() {
            Ok(principal)
        } else {
            Err(Status::permission_denied(format!(
                "principal {:?} is not an admin; {operation} requires admin",
                principal.name()
            )))
        }
    }

    /// Authenticate, parse a `PutRequest` (malformed → `InvalidArgument`) and
    /// authorize `Write` on the record's namespace. Shared by `Put` and `PutRaw`,
    /// in #146 order.
    #[allow(clippy::result_large_err)]
    fn authorize_put(
        &self,
        metadata: &MetadataMap,
        r: &PutRequest,
    ) -> Result<(Principal, Record, Option<Revision>), Status> {
        let principal = self.authenticate(metadata)?;
        let record: Record = serde_json::from_slice(&r.record_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let expected: Option<Revision> = serde_json::from_slice(&r.expected_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.check_access(&principal, Access::Write, &record.key.namespace)?;
        Ok((principal, record, expected))
    }
```

Replace `get`, `put`, `delete` and `list` (lines 108-210) with:

```rust
    async fn get(&self, req: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.namespace)?;
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        let rec = self.service.get(&key).await.map_err(internal)?;
        Ok(Response::new(get_response(rec)?))
    }

    async fn put(&self, req: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Authenticate BEFORE deserializing attacker-controlled JSON (#146): an
        // unauthenticated caller is rejected without ever feeding its body to
        // serde. Only then parse the body (malformed input is the caller's
        // error → invalid_argument, not internal) and authorize the write
        // against the namespace named in the record's key.
        let (principal, mut record, expected) = self.authorize_put(&metadata, &r)?;
        // Stamp the author from the authenticated principal — unforgeable (ADR
        // 0015). Open mode (no auth) leaves the record's author untouched.
        if principal.is_authenticated() {
            record.meta.author = Identity::new(principal.name());
        }
        let outcome = self
            .service
            .put(record, expected)
            .await
            .map_err(put_error)?;
        Ok(Response::new(put_response(outcome)?))
    }

    async fn delete(
        &self,
        req: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Authenticate BEFORE deserializing attacker-controlled JSON (#146), then
        // parse the precondition (malformed input is the caller's error →
        // invalid_argument), authorize the write against the path's namespace,
        // and build the key from (namespace, collection, id).
        let principal = self.authenticate(&metadata)?;
        let expected: Option<Revision> = serde_json::from_slice(&r.expected_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.check_access(&principal, Access::Write, &r.namespace)?;
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        // The store writes a tombstone (gonzalo#203) stamped with the
        // authenticated principal, as `put` stamps; open mode passes `None`.
        let author = principal
            .is_authenticated()
            .then(|| Identity::new(principal.name()));
        let outcome = self
            .service
            .delete_as(&key, expected, author)
            .await
            .map_err(internal)?;
        let (outcome, payload_json) = delete_outcome_parts(outcome)?;
        Ok(Response::new(DeleteResponse {
            outcome,
            payload_json,
        }))
    }

    async fn list(&self, req: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Listing without a namespace spans all namespaces → requires admin
        // (`read` on `"*"`); a namespaced list needs read on that namespace.
        self.authorize(
            &metadata,
            Access::Read,
            r.namespace.as_deref().unwrap_or("*"),
        )?;
        let prefix = KeyPrefix {
            namespace: r.namespace,
            collection: r.collection,
        };
        let keys = self.service.list(&prefix).await.map_err(internal)?;
        Ok(Response::new(list_response(&keys)?))
    }

    async fn get_raw(&self, req: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.namespace)?;
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        let rec = self.service.get_raw(&key).await.map_err(internal)?;
        Ok(Response::new(get_response(rec)?))
    }

    async fn list_raw(
        &self,
        req: Request<ListRequest>,
    ) -> Result<Response<ListResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Same authorization as `List`: unscoped requires `read` on `"*"`.
        self.authorize(
            &metadata,
            Access::Read,
            r.namespace.as_deref().unwrap_or("*"),
        )?;
        let prefix = KeyPrefix {
            namespace: r.namespace,
            collection: r.collection,
        };
        let keys = self.service.list_raw(&prefix).await.map_err(internal)?;
        Ok(Response::new(list_response(&keys)?))
    }

    async fn put_raw(&self, req: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Replication write (gonzalo#203). Authorship stays unforgeable (ADR
        // 0015): an admin token is the replication credential, so an admin's
        // PutRaw keeps the replicated record's original author. Any other
        // principal is restamped exactly as `Put` restamps. Open mode's
        // implicit principal is an admin, so it keeps the author too.
        let (principal, mut record, expected) = self.authorize_put(&metadata, &r)?;
        if !principal.is_admin() {
            record.meta.author = Identity::new(principal.name());
        }
        let outcome = self
            .service
            .put_raw(record, expected)
            .await
            .map_err(put_error)?;
        Ok(Response::new(put_response(outcome)?))
    }

    async fn purge(&self, req: Request<PurgeRequest>) -> Result<Response<PurgeResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Admin check first, then parse the attacker-controlled precondition
        // (malformed → invalid_argument, the caller's error).
        self.authorize_admin(&metadata, "purge")?;
        let expected: Revision = serde_json::from_slice(&r.expected_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        let outcome = self
            .service
            .purge(&key, expected)
            .await
            .map_err(internal)?;
        let (outcome, payload_json) = delete_outcome_parts(outcome)?;
        Ok(Response::new(PurgeResponse {
            outcome,
            payload_json,
        }))
    }
```

Before `located_response` (line 377) add:

```rust
/// Map a put failure. `CoreError::NotFound` (`expected` names a revision the
/// store does not hold) is the caller's precondition failing →
/// `FailedPrecondition`, which the client maps back to `NotFound`. Everything
/// else is opaque (#148).
fn put_error(e: CoreError) -> Status {
    match e {
        CoreError::NotFound(key) => Status::failed_precondition(format!("record not found: {key}")),
        other => internal(other),
    }
}

/// `PutResponse` for a `PutResult` (shared by `Put` and `PutRaw`).
#[allow(clippy::result_large_err)]
fn put_response(outcome: PutResult) -> Result<PutResponse, Status> {
    Ok(match outcome {
        PutResult::Committed(rev) => PutResponse {
            outcome: "committed".into(),
            payload_json: serde_json::to_vec(&rev).map_err(internal)?,
        },
        PutResult::Conflict(c) => PutResponse {
            outcome: "conflict".into(),
            payload_json: serde_json::to_vec(&*c).map_err(internal)?,
        },
    })
}

/// `GetResponse` for an optional record (shared by `Get` and `GetRaw`).
#[allow(clippy::result_large_err)]
fn get_response(rec: Option<Record>) -> Result<GetResponse, Status> {
    Ok(match rec {
        Some(rec) => GetResponse {
            found: true,
            record_json: serde_json::to_vec(&rec).map_err(internal)?,
        },
        None => GetResponse {
            found: false,
            record_json: Vec::new(),
        },
    })
}

/// `ListResponse` of JSON-encoded keys (shared by `List` and `ListRaw`).
#[allow(clippy::result_large_err)]
fn list_response(keys: &[RecordKey]) -> Result<ListResponse, Status> {
    let keys_json = keys
        .iter()
        .map(serde_json::to_vec)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(internal)?;
    Ok(ListResponse { keys_json })
}

/// `(outcome, payload_json)` for a `DeleteResult` (shared by `Delete` and
/// `Purge`): `"deleted"` + empty, or `"conflict"` + JSON of `Conflict`.
#[allow(clippy::result_large_err)]
fn delete_outcome_parts(outcome: DeleteResult) -> Result<(String, Vec<u8>), Status> {
    Ok(match outcome {
        DeleteResult::Deleted => ("deleted".into(), Vec::new()),
        DeleteResult::Conflict(c) => (
            "conflict".into(),
            serde_json::to_vec(&*c).map_err(internal)?,
        ),
    })
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p gonzalo-server --lib`
Expected: PASS. New gRPC tests:

- `grpc_delete_leaves_a_stamped_tombstone_for_raw_reads`
- `grpc_raw_reads_are_read_scoped`
- `grpc_unscoped_list_raw_requires_admin`
- `grpc_put_raw_is_write_scoped`
- `grpc_put_raw_by_scoped_writer_is_stamped_with_the_writer`
- `grpc_put_raw_by_admin_keeps_the_incoming_author`
- `grpc_put_raw_in_open_mode_keeps_the_incoming_author`
- `grpc_put_raw_over_tombstone_none_conflicts_some_overwrites`
- `grpc_put_not_found_is_failed_precondition`
- `grpc_purge_requires_admin`
- `grpc_purge_stale_expected_is_conflict`
- `grpc_purge_authorizes_before_parsing`
- `grpc_raw_backend_error_is_opaque`

Every pre-existing gRPC test also passes (`put_authenticates_before_deserializing`, `put_malformed_body_is_invalid_argument_not_internal`, `write_is_scoped_and_author_is_stamped`, and the rest).

Run: `cargo build -p gonzalo-store-server`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-proto/proto/gonzalo.proto crates/gonzalo-proto/src/lib.rs crates/gonzalo-server/src/grpc.rs
git commit -m "feat(server): gRPC GetRaw, ListRaw, PutRaw, admin Purge; stamped deletes (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 4: `ServerStore` replication methods, `delete_as`, upgrade error

**Files:**
- Modify: `crates/gonzalo-store-server/Cargo.toml` (add `[dev-dependencies]`, update the comment at lines 20-24)
- Modify: `crates/gonzalo-store-server/src/lib.rs`:
  - imports `:8-17`
  - URL helpers `:78-100`
  - `put` `:161-205`
  - `list` `:207-250`
  - `delete` `:252-296` (becomes `delete_as`)
  - slice-1 interim `get_raw`/`list_raw`/`put_raw`/`purge` (replace)
  - classify helpers near `:427-533`
  - tests
- Test: `crates/gonzalo-store-server/src/lib.rs` inline tests

**Interfaces:**
- Consumes: HTTP routes and DTOs (Task 2); gRPC client methods (Task 3).
- Produces:
  - `pub const gonzalo_store_server::DAEMON_PREDATES_REPLICATION: &str = "daemon predates replication reads (gonzalo#203); upgrade gonzalod";`
  - `impl Store for ServerStore`: real `get_raw`, `list_raw`, `put_raw` and `purge` on both transports. HTTP `404` or gRPC `Code::Unimplemented` → `CoreError::Backend(DAEMON_PREDATES_REPLICATION.into())`.
  - `delete_as` sends the delete and **ignores `author`**, because the daemon stamps from the bearer token.
  - `put`/`put_raw`: HTTP `412` or gRPC `FailedPrecondition` → `CoreError::NotFound(record.key)`.

- [ ] **Step 1: Add dev-dependencies**

In `crates/gonzalo-store-server/Cargo.toml`, replace lines 20-24 (the comment block) with:

```toml
# Cross-crate integration tests (client ↔ real gonzalod ↔ store, over HTTP/gRPC)
# live in the `gonzalo-integration-tests` crate, not here — so this published
# library carries no dev-dep on the gonzalo-server binary or gonzalo-graph, and
# its crates.io publish order tracks only its runtime deps (gonzalo#190). The
# inline unit tests use only third-party dev-deps: wiremock stands in for an
# old HTTP daemon and a service-less tonic server for an old gRPC daemon
# (gonzalo#203).
[dev-dependencies]
tokio        = { workspace = true, features = ["rt", "macros", "net"] }
tokio-stream = { workspace = true }
wiremock     = { workspace = true }
```

- [ ] **Step 2: Write the failing tests**

Replace `sample_record` in the test module (lines 543-560) with the version below. If slice 1 already added the two new fields, only the field order changes.

```rust
    fn sample_record() -> Record {
        let body = Body::Inline(b"hello".to_vec());
        Record {
            key: RecordKey::new("ns", "col", "id"),
            kind: RecordKind::Topic,
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            meta: Meta {
                author: Identity::new("tester"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            ancestors: Vec::new(),
            deleted_at: None,
        }
    }
```

Append inside `mod tests`:

```rust
    // ── replication surface: upgrade error, never a fallback (#203) ─────────

    fn upgrade_error() -> String {
        CoreError::Backend(DAEMON_PREDATES_REPLICATION.into()).to_string()
    }

    #[test]
    fn raw_get_200_null_is_absent_and_200_record_is_present() {
        assert_eq!(
            classify_raw_get_response(StatusCode::OK, r#"{"record":null}"#).unwrap(),
            None
        );
        let rec = sample_record();
        let body = serde_json::to_string(&RawRecordBody {
            record: Some(rec.clone()),
        })
        .unwrap();
        assert_eq!(
            classify_raw_get_response(StatusCode::OK, &body).unwrap(),
            Some(rec)
        );
    }

    #[test]
    fn replication_404_is_the_upgrade_error() {
        let key = RecordKey::new("ns", "col", "id");
        for err in [
            classify_raw_get_response(StatusCode::NOT_FOUND, "")
                .unwrap_err()
                .to_string(),
            classify_raw_list_response(StatusCode::NOT_FOUND, "")
                .unwrap_err()
                .to_string(),
            classify_raw_put_response(&key, StatusCode::NOT_FOUND, "")
                .unwrap_err()
                .to_string(),
            classify_purge_response(StatusCode::NOT_FOUND, "")
                .unwrap_err()
                .to_string(),
        ] {
            assert_eq!(err, upgrade_error());
        }
    }

    #[test]
    fn put_412_is_not_found_for_the_record_key() {
        let key = RecordKey::new("ns", "col", "id");
        assert!(matches!(
            classify_put_response_for(&key, StatusCode::PRECONDITION_FAILED, "record not found"),
            Err(CoreError::NotFound(k)) if k == key
        ));
        assert!(matches!(
            classify_raw_put_response(&key, StatusCode::PRECONDITION_FAILED, "record not found"),
            Err(CoreError::NotFound(k)) if k == key
        ));
        // The consumer put route never turns a 404 into the upgrade error.
        let msg = classify_put_response_for(&key, StatusCode::NOT_FOUND, "nope")
            .unwrap_err()
            .to_string();
        assert_ne!(msg, upgrade_error());
    }

    #[test]
    fn replication_403_keeps_the_daemon_body() {
        let body = "principal \"w\" is not an admin; purge requires admin";
        let msg = classify_purge_response(StatusCode::FORBIDDEN, body)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("403") && msg.contains(body), "{msg}");
        let msg = classify_raw_list_response(StatusCode::FORBIDDEN, "nope")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("403") && msg.contains("nope"), "{msg}");
    }

    #[test]
    fn purge_200_and_409_parse_delete_outcomes() {
        let ok = serde_json::to_string(&DeleteOutcome::Deleted).unwrap();
        assert_eq!(
            classify_purge_response(StatusCode::OK, &ok).unwrap(),
            DeleteResult::Deleted
        );
        let rec = sample_record();
        let conflict = serde_json::to_string(&DeleteOutcome::Conflict {
            conflict: Box::new(Conflict {
                key: rec.key.clone(),
                expected: None,
                current: rec,
            }),
        })
        .unwrap();
        assert!(matches!(
            classify_purge_response(StatusCode::CONFLICT, &conflict).unwrap(),
            DeleteResult::Conflict(_)
        ));
    }

    #[test]
    fn grpc_status_mapping_for_replication_and_puts() {
        let key = RecordKey::new("ns", "col", "id");
        assert_eq!(
            replication_status(tonic::Status::unimplemented("GetRaw")).to_string(),
            upgrade_error()
        );
        assert_ne!(
            replication_status(tonic::Status::permission_denied("no")).to_string(),
            upgrade_error()
        );
        assert!(matches!(
            put_status(tonic::Status::failed_precondition("record not found"), &key),
            CoreError::NotFound(k) if k == key
        ));
        assert_eq!(
            raw_put_status(tonic::Status::unimplemented("PutRaw"), &key).to_string(),
            upgrade_error()
        );
        assert!(matches!(
            raw_put_status(tonic::Status::failed_precondition("x"), &key),
            CoreError::NotFound(_)
        ));
    }

    /// Spec §6.5: against a daemon without the replication routes, every
    /// replication call fails with the upgrade error and **no consumer route is
    /// ever hit**.
    #[tokio::test]
    async fn http_old_daemon_errors_and_never_falls_back_to_consumer_routes() {
        use wiremock::matchers::path_regex;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Any consumer-route hit is a fallback: verification fails the test.
        Mock::given(path_regex(r"^/v1/(records|keys)(/|$)"))
            .respond_with(ResponseTemplate::new(200))
            .named("consumer route (fallback)")
            .expect(0)
            .mount(&server)
            .await;
        // An old daemon has no replication routes.
        Mock::given(path_regex(r"^/v1/(raw|purge)/"))
            .respond_with(ResponseTemplate::new(404))
            .named("replication route")
            .expect(4)
            .mount(&server)
            .await;

        let store = ServerStore::http(&server.uri()).unwrap();
        let key = RecordKey::new("ns", "col", "id");
        let errors = [
            store.get_raw(&key).await.unwrap_err(),
            store.list_raw(&KeyPrefix::default()).await.unwrap_err(),
            store.put_raw(sample_record(), None).await.unwrap_err(),
            store
                .purge(&key, Revision::initial(b"x"))
                .await
                .unwrap_err(),
        ];
        for err in errors {
            assert_eq!(err.to_string(), upgrade_error());
        }
        server.verify().await;
    }

    /// A tonic server with no services answers every RPC `Unimplemented`,
    /// which is what a pre-#203 daemon does for the replication RPCs.
    #[tokio::test]
    async fn grpc_old_daemon_errors_with_the_upgrade_message() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_routes(tonic::service::Routes::default())
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );
        let store = ServerStore::grpc(format!("http://{addr}")).await.unwrap();
        let key = RecordKey::new("ns", "col", "id");
        let errors = [
            store.get_raw(&key).await.unwrap_err(),
            store.list_raw(&KeyPrefix::default()).await.unwrap_err(),
            store.put_raw(sample_record(), None).await.unwrap_err(),
            store
                .purge(&key, Revision::initial(b"x"))
                .await
                .unwrap_err(),
        ];
        for err in errors {
            assert_eq!(err.to_string(), upgrade_error());
        }
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p gonzalo-store-server --lib`
Expected: compile errors: `cannot find value `DAEMON_PREDATES_REPLICATION``, `cannot find function `classify_raw_get_response``, `cannot find function `put_status``.

- [ ] **Step 4: Implement**

In `crates/gonzalo-store-server/src/lib.rs`:

Replace imports at lines 8-17:

```rust
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, PutResult, Record,
    RecordKey, Result, Revision, Store, store::Conflict,
};
use gonzalo_proto::http::{
    DeleteBody, DeleteOutcome, PurgeBody, PutBody, PutOutcome, RawRecordBody,
};
use gonzalo_proto::v1::{
    DeleteBlobRequest, DeleteRequest, GetBlobRequest, GetRequest, ListBlobsRequest, ListRequest,
    ListResponse, PurgeRequest, PutBlobRequest, PutRequest, PutResponse,
    gonzalo_client::GonzaloClient,
};
use tonic::transport::Channel;

/// The error every replication call (`get_raw`, `list_raw`, `put_raw`,
/// `purge`) returns against a daemon without the replication surface: HTTP
/// `404` on the route, or gRPC `Unimplemented`. There is deliberately **no
/// fallback** to the consumer routes. Consumer reads hide tombstones and
/// consumer put re-stamps recreations, so replicating through them resurrects
/// deleted records (spec §3.6).
pub const DAEMON_PREDATES_REPLICATION: &str =
    "daemon predates replication reads (gonzalo#203); upgrade gonzalod";
```

In `impl ServerStore`, replace `records_url` (lines 78-84) with:

```rust
    /// `base` + `segments` + the record key's three segments.
    fn key_url(base: &reqwest::Url, segments: &[&str], key: &RecordKey) -> Result<reqwest::Url> {
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|_| CoreError::Backend("base URL cannot be a base".into()))?
            .extend(segments)
            .extend([&key.namespace, &key.collection, &key.id]);
        Ok(url)
    }

    fn records_url(base: &reqwest::Url, key: &RecordKey) -> Result<reqwest::Url> {
        Self::key_url(base, &["v1", "records"], key)
    }

    /// `…/v1/raw/records/{ns}/{col}/{id}` (gonzalo#203).
    fn raw_records_url(base: &reqwest::Url, key: &RecordKey) -> Result<reqwest::Url> {
        Self::key_url(base, &["v1", "raw", "records"], key)
    }

    /// `…/v1/purge/{ns}/{col}/{id}` (gonzalo#203).
    fn purge_url(base: &reqwest::Url, key: &RecordKey) -> Result<reqwest::Url> {
        Self::key_url(base, &["v1", "purge"], key)
    }

    /// `base` + `segments` + `?namespace=&collection=` from `prefix` (shared by
    /// `/v1/keys` and `/v1/raw/keys`).
    fn keys_url(
        base: &reqwest::Url,
        segments: &[&str],
        prefix: &KeyPrefix,
    ) -> Result<reqwest::Url> {
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|_| CoreError::Backend("base URL cannot be a base".into()))?
            .extend(segments);
        {
            let mut q = url.query_pairs_mut();
            if let Some(ns) = &prefix.namespace {
                q.append_pair("namespace", ns);
            }
            if let Some(col) = &prefix.collection {
                q.append_pair("collection", col);
            }
        }
        Ok(url)
    }
```

Replace `put` (lines 161-205) with:

```rust
    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let key = record.key.clone();
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::records_url(base, &key)?;
                let body = PutBody { record, expected };
                let resp = maybe_auth(client.put(url).json(&body), token)
                    .send()
                    .await
                    .map_err(be)?;
                // Read the body as text first so error statuses (403/413/400,
                // which carry plain-text bodies) surface their real status and
                // message instead of being masked as a JSON decode error (#147).
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_put_response_for(&key, status, &text)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    PutRequest {
                        record_json: serde_json::to_vec(&record).map_err(se)?,
                        expected_json: serde_json::to_vec(&expected).map_err(se)?,
                    },
                    token,
                )?;
                let resp = client
                    .put(req)
                    .await
                    .map_err(|s| put_status(s, &key))?
                    .into_inner();
                decode_put_response(resp)
            }
        }
    }
```

In `list`'s HTTP arm, replace lines 214-226 (from `let mut url = base.clone();` through the closing brace of the query block) with:

```rust
                let url = Self::keys_url(base, &["v1", "keys"], prefix)?;
```

In `list`'s gRPC arm, replace lines 244-247 (`resp.keys_json ... .collect()`) with:

```rust
                decode_keys(resp)
```

Rename `async fn delete(&self, key: &RecordKey, expected: Option<Revision>)` at line 252 to the following signature, and add the doc comment. The body is unchanged. If slice 1 already renamed it, only add the comment and the `_author` name.

```rust
    /// Sends the delete. `_author` is ignored on purpose: the daemon stamps the
    /// tombstone from the bearer token (ADR 0015), which a client cannot forge.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        _author: Option<Identity>,
    ) -> Result<DeleteResult> {
```

Replace the slice-1 interim replication methods in `impl Store for ServerStore` (find them with `rg -n "async fn (get_raw|list_raw|put_raw|purge)" crates/gonzalo-store-server/src/lib.rs`) with:

```rust
    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::raw_records_url(base, key)?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_raw_get_response(status, &text)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    GetRequest {
                        namespace: key.namespace.clone(),
                        collection: key.collection.clone(),
                        id: key.id.clone(),
                    },
                    token,
                )?;
                let resp = client
                    .get_raw(req)
                    .await
                    .map_err(replication_status)?
                    .into_inner();
                if resp.found {
                    Ok(Some(serde_json::from_slice(&resp.record_json).map_err(se)?))
                } else {
                    Ok(None)
                }
            }
        }
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::keys_url(base, &["v1", "raw", "keys"], prefix)?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_raw_list_response(status, &text)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    ListRequest {
                        namespace: prefix.namespace.clone(),
                        collection: prefix.collection.clone(),
                    },
                    token,
                )?;
                let resp = client
                    .list_raw(req)
                    .await
                    .map_err(replication_status)?
                    .into_inner();
                decode_keys(resp)
            }
        }
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let key = record.key.clone();
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::raw_records_url(base, &key)?;
                let body = PutBody { record, expected };
                let resp = maybe_auth(client.put(url).json(&body), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_raw_put_response(&key, status, &text)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    PutRequest {
                        record_json: serde_json::to_vec(&record).map_err(se)?,
                        expected_json: serde_json::to_vec(&expected).map_err(se)?,
                    },
                    token,
                )?;
                let resp = client
                    .put_raw(req)
                    .await
                    .map_err(|s| raw_put_status(s, &key))?
                    .into_inner();
                decode_put_response(resp)
            }
        }
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::purge_url(base, key)?;
                let body = PurgeBody { expected };
                let resp = maybe_auth(client.post(url).json(&body), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_purge_response(status, &text)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    PurgeRequest {
                        namespace: key.namespace.clone(),
                        collection: key.collection.clone(),
                        id: key.id.clone(),
                        expected_json: serde_json::to_vec(&expected).map_err(se)?,
                    },
                    token,
                )?;
                let resp = client
                    .purge(req)
                    .await
                    .map_err(replication_status)?
                    .into_inner();
                match resp.outcome.as_str() {
                    "deleted" => Ok(DeleteResult::Deleted),
                    "conflict" => {
                        let c: Conflict = serde_json::from_slice(&resp.payload_json).map_err(se)?;
                        Ok(DeleteResult::Conflict(Box::new(c)))
                    }
                    other => Err(CoreError::Backend(format!(
                        "unknown purge outcome: {other}"
                    ))),
                }
            }
        }
    }
```

After `classify_put_response` (ends line 477) add:

```rust
/// [`classify_put_response`], plus `412 Precondition Failed`: the daemon's
/// `CoreError::NotFound` (`expected` names a revision the store does not hold),
/// restored as `NotFound(key)`.
fn classify_put_response_for(
    key: &RecordKey,
    status: reqwest::StatusCode,
    body: &str,
) -> Result<PutResult> {
    match status {
        reqwest::StatusCode::PRECONDITION_FAILED => Err(CoreError::NotFound(key.clone())),
        other => classify_put_response(other, body),
    }
}

/// A raw put's result: `404` means the route does not exist →
/// [`upgrade_required`], never a fallback; otherwise as a consumer put.
fn classify_raw_put_response(
    key: &RecordKey,
    status: reqwest::StatusCode,
    body: &str,
) -> Result<PutResult> {
    match status {
        reqwest::StatusCode::NOT_FOUND => Err(upgrade_required()),
        other => classify_put_response_for(key, other, body),
    }
}

/// Decode a gRPC `PutResponse` (shared by `Put` and `PutRaw`).
fn decode_put_response(resp: PutResponse) -> Result<PutResult> {
    match resp.outcome.as_str() {
        "committed" => {
            let rev: Revision = serde_json::from_slice(&resp.payload_json).map_err(se)?;
            Ok(PutResult::Committed(rev))
        }
        "conflict" => {
            let c: Conflict = serde_json::from_slice(&resp.payload_json).map_err(se)?;
            Ok(PutResult::Conflict(Box::new(c)))
        }
        other => Err(CoreError::Backend(format!("unknown put outcome: {other}"))),
    }
}

/// [`DAEMON_PREDATES_REPLICATION`] as a `CoreError`.
fn upgrade_required() -> CoreError {
    CoreError::Backend(DAEMON_PREDATES_REPLICATION.into())
}

/// Decide a raw `get` from the HTTP status and body text. `200` carries a
/// [`RawRecordBody`] (absence is `{"record": null}`); `404` →
/// [`upgrade_required`]; any other status surfaces the daemon's body (#195).
fn classify_raw_get_response(status: reqwest::StatusCode, body: &str) -> Result<Option<Record>> {
    match status {
        reqwest::StatusCode::OK => Ok(serde_json::from_str::<RawRecordBody>(body)
            .map_err(se)?
            .record),
        reqwest::StatusCode::NOT_FOUND => Err(upgrade_required()),
        other => Err(read_response_error(other, body)),
    }
}

/// Decide a raw `list`: `200` → keys, `404` → [`upgrade_required`], otherwise
/// the daemon's body.
fn classify_raw_list_response(status: reqwest::StatusCode, body: &str) -> Result<Vec<RecordKey>> {
    match status {
        reqwest::StatusCode::OK => serde_json::from_str(body).map_err(se),
        reqwest::StatusCode::NOT_FOUND => Err(upgrade_required()),
        other => Err(read_response_error(other, body)),
    }
}

/// Decide a `purge`: `200`/`409` carry a [`DeleteOutcome`], `404` →
/// [`upgrade_required`], and any other status (`403` non-admin, `400` bad body)
/// surfaces verbatim, as `delete` does.
fn classify_purge_response(status: reqwest::StatusCode, body: &str) -> Result<DeleteResult> {
    match status {
        reqwest::StatusCode::NOT_FOUND => Err(upgrade_required()),
        other => classify_delete_response(other, body),
    }
}

/// Map a gRPC failure on a replication RPC: `Unimplemented` means the daemon
/// has no such RPC → [`upgrade_required`]; anything else maps as usual.
fn replication_status(s: tonic::Status) -> CoreError {
    if s.code() == tonic::Code::Unimplemented {
        upgrade_required()
    } else {
        status(s)
    }
}

/// Map a gRPC failure on `Put`: `FailedPrecondition` is the daemon's
/// `CoreError::NotFound`, restored as `NotFound(key)`.
fn put_status(s: tonic::Status, key: &RecordKey) -> CoreError {
    if s.code() == tonic::Code::FailedPrecondition {
        CoreError::NotFound(key.clone())
    } else {
        status(s)
    }
}

/// Map a gRPC failure on `PutRaw`: `Unimplemented` → [`upgrade_required`],
/// otherwise as [`put_status`].
fn raw_put_status(s: tonic::Status, key: &RecordKey) -> CoreError {
    if s.code() == tonic::Code::Unimplemented {
        upgrade_required()
    } else {
        put_status(s, key)
    }
}

/// Decode a `ListResponse`'s JSON keys (shared by `List` and `ListRaw`).
fn decode_keys(resp: ListResponse) -> Result<Vec<RecordKey>> {
    resp.keys_json
        .iter()
        .map(|b| serde_json::from_slice::<RecordKey>(b).map_err(se))
        .collect()
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p gonzalo-store-server --lib`
Expected: PASS. New tests:

- `raw_get_200_null_is_absent_and_200_record_is_present`
- `replication_404_is_the_upgrade_error`
- `put_412_is_not_found_for_the_record_key`
- `replication_403_keeps_the_daemon_body`
- `purge_200_and_409_parse_delete_outcomes`
- `grpc_status_mapping_for_replication_and_puts`
- `http_old_daemon_errors_and_never_falls_back_to_consumer_routes`
- `grpc_old_daemon_errors_with_the_upgrade_message`

The pre-existing `classify_put_response` tests are untouched and still pass.

Sanity-check the fallback guard: temporarily change `raw_records_url` to use `&["v1", "records"]`, rerun `cargo test -p gonzalo-store-server --lib http_old_daemon`, and confirm it FAILS on the `consumer route (fallback)` expectation. Then revert the change.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-store-server/Cargo.toml crates/gonzalo-store-server/src/lib.rs Cargo.lock
git commit -m "feat(store-server): real replication methods with upgrade error (#203)

get_raw/list_raw/put_raw/purge over both transports. HTTP 404 and gRPC
Unimplemented map to \"daemon predates replication reads (gonzalo#203);
upgrade gonzalod\" with no consumer fallback (wiremock-asserted). Put
NotFound round-trips as 412 / FailedPrecondition. delete_as leaves
authorship to the daemon's token.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 5: Tombstone conformance over the daemon

The existing HTTP/gRPC conformance tests share **one** daemon across every factory call (`server_store_conformance.rs:22-50`). The reconciled contract requires a fresh, empty store on every call, so every factory now stands up its own daemon, as the blob factories already do (`:52-70`).

**Files:**
- Modify (full rewrite): `crates/gonzalo-integration-tests/tests/server_store_conformance.rs`

**Interfaces:**
- Consumes: `run_tombstone_conformance(factory, cap)` and `DEFAULT_ANCESTOR_CAP` (slice 1); `FsStore::with_ancestor_cap(self, usize) -> gonzalo_core::Result<Self>` (slice 2); `ServerStore` replication methods and `DAEMON_PREDATES_REPLICATION` (Task 4); daemon routes (Tasks 2–3).
- Produces: `fresh_http_store(cap)`, `fresh_grpc_store(cap)` test helpers (file-local), reused by Task 7.

- [ ] **Step 1: Write the tests**

Replace the whole of `crates/gonzalo-integration-tests/tests/server_store_conformance.rs` with:

```rust
//! End-to-end: a daemon backed by a filesystem store must serve a remote
//! `ServerStore` that passes the shared conformance suites — over BOTH the
//! HTTP/JSON and gRPC transports. Every factory call stands up a fresh daemon
//! over a fresh `FsStore`, because the suites require a fresh, empty store per
//! invocation.

use gonzalo_core::DEFAULT_ANCESTOR_CAP;
use gonzalo_core::conformance::{
    run_blob_store_conformance, run_store_conformance, run_tombstone_conformance,
};
use gonzalo_server::{Auth, Principal, Service, serve_grpc, serve_http};
use gonzalo_store_fs::FsStore;
use gonzalo_store_server::ServerStore;
use std::sync::Arc;
use tokio::net::TcpListener;

/// A small cap so `ancestors_capped_and_ordered` exercises truncation over the
/// wire in a handful of writes.
const SMALL_CAP: usize = 3;

fn service_with_cap(cap: usize) -> Service {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let fs = Arc::new(
        FsStore::new(dir)
            .with_ancestor_cap(cap)
            .expect("valid ancestor cap"),
    );
    Service::new(fs.clone(), fs)
}

fn open() -> Arc<Auth> {
    Arc::new(Auth::Disabled)
}

/// A fresh open daemon over HTTP whose backing store uses `cap`.
async fn fresh_http_store(cap: usize) -> ServerStore {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(listener, service_with_cap(cap), open()));
    ServerStore::http(&format!("http://{addr}")).unwrap()
}

/// As `fresh_http_store`, over gRPC (waits briefly for the server to accept).
async fn fresh_grpc_store(cap: usize) -> ServerStore {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_grpc(listener, service_with_cap(cap), open()));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    ServerStore::grpc(format!("http://{addr}")).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_conformance() {
    run_store_conformance(|| fresh_http_store(DEFAULT_ANCESTOR_CAP)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_conformance() {
    run_store_conformance(|| fresh_grpc_store(DEFAULT_ANCESTOR_CAP)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_tombstone_conformance() {
    run_tombstone_conformance(|| fresh_http_store(DEFAULT_ANCESTOR_CAP), DEFAULT_ANCESTOR_CAP)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_tombstone_conformance() {
    run_tombstone_conformance(|| fresh_grpc_store(DEFAULT_ANCESTOR_CAP), DEFAULT_ANCESTOR_CAP)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_tombstone_conformance_small_cap() {
    run_tombstone_conformance(|| fresh_http_store(SMALL_CAP), SMALL_CAP).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_tombstone_conformance_small_cap() {
    run_tombstone_conformance(|| fresh_grpc_store(SMALL_CAP), SMALL_CAP).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_blob_conformance() {
    run_blob_store_conformance(|| fresh_http_store(DEFAULT_ANCESTOR_CAP)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_blob_conformance() {
    run_blob_store_conformance(|| fresh_grpc_store(DEFAULT_ANCESTOR_CAP)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_auth_rejects_wrong_token_and_accepts_correct() {
    use gonzalo_core::{KeyPrefix, Store};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let auth = Arc::new(Auth::Enabled(std::collections::HashMap::from([(
        "s3cret".to_string(),
        Principal::admin("admin"),
    )])));
    tokio::spawn(serve_http(
        listener,
        service_with_cap(DEFAULT_ANCESTOR_CAP),
        auth,
    ));
    let base = format!("http://{addr}");

    // No token / wrong token -> error (401 surfaced as a backend error).
    let anon = ServerStore::http(&base).unwrap();
    assert!(anon.list(&KeyPrefix::default()).await.is_err());
    let wrong = ServerStore::http_with_token(&base, "nope").unwrap();
    assert!(wrong.list(&KeyPrefix::default()).await.is_err());

    // Correct admin token -> ok (admin may list across all namespaces).
    let ok = ServerStore::http_with_token(&base, "s3cret").unwrap();
    assert!(ok.list(&KeyPrefix::default()).await.is_ok());
}

/// Purge over a real daemon is admin-only end to end: a namespace writer's
/// `ServerStore::purge` fails with the daemon's 403, not the upgrade error.
#[tokio::test(flavor = "multi_thread")]
async fn http_purge_by_non_admin_is_forbidden_end_to_end() {
    use gonzalo_core::{Revision, Store};
    use gonzalo_store_server::DAEMON_PREDATES_REPLICATION;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let auth = Arc::new(Auth::Enabled(std::collections::HashMap::from([(
        "wtok".to_string(),
        Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
    )])));
    tokio::spawn(serve_http(
        listener,
        service_with_cap(DEFAULT_ANCESTOR_CAP),
        auth,
    ));
    let writer = ServerStore::http_with_token(&format!("http://{addr}"), "wtok").unwrap();
    let key = gonzalo_core::RecordKey::new("memory", "col", "x");
    let msg = writer
        .purge(&key, Revision::initial(b"x"))
        .await
        .unwrap_err()
        .to_string();
    assert!(msg.contains("403"), "{msg}");
    assert!(!msg.contains(DAEMON_PREDATES_REPLICATION), "{msg}");
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p gonzalo-integration-tests --test server_store_conformance`
Expected: PASS, 11 tests.

If a tombstone case fails over the wire but passes for `FsStore` directly, the bug is in Tasks 2–4:

- a JSON round-trip dropping `ancestors`/`deleted_at` (check slice 1's `Record` serde attributes);
- `put_some_over_tombstone_is_not_found` getting `Backend` instead of `NotFound` (check the 412 / `FailedPrecondition` mapping in both directions).

- [ ] **Step 3: Commit**

```bash
git add crates/gonzalo-integration-tests/tests/server_store_conformance.rs
git commit -m "test(integration): tombstone conformance over the daemon (#203)

Every factory call now stands up a fresh daemon, as the conformance
contract requires.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 6: `GONZALO_ANCESTOR_CAP` for `gonzalod`

`gonzalod` is configured entirely by `GONZALO_*` environment variables (`gonzalod.rs:1-22`, `config.rs:24-55`), and has no command-line flags. The ancestor cap follows that convention: **environment variable only**, read right next to `StoreConfig::from_env`. Argument handling is unchanged: no flag, and unknown arguments are not rejected.

**Files:**
- Modify: `crates/gonzalo-server/src/config.rs` (add fn after `impl StoreConfig`, line 56; tests)
- Modify: `crates/gonzalo-server/src/lib.rs:12`
- Modify: `crates/gonzalo-server/src/bin/gonzalod.rs:1-22` (docs), `:24-25` (imports), `:45-69` (wiring), `:90-93` (startup line)

**Interfaces:**
- Consumes: `gonzalo_core::{DEFAULT_ANCESTOR_CAP, validate_ancestor_cap}` (slice 1); `FsStore::with_ancestor_cap` and `S3Store::with_ancestor_cap` → `gonzalo_core::Result<Self>` (slices 2, 3).
- Produces: `pub fn gonzalo_server::ancestor_cap_from_env(get: impl Fn(&str) -> Option<String>) -> Result<usize, String>`.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `crates/gonzalo-server/src/config.rs`:

```rust
    #[test]
    fn ancestor_cap_defaults_when_unset_or_empty() {
        assert_eq!(
            ancestor_cap_from_env(env(&[])).unwrap(),
            gonzalo_core::DEFAULT_ANCESTOR_CAP
        );
        // An empty value is unset, like the other GONZALO_* knobs.
        assert_eq!(
            ancestor_cap_from_env(env(&[("GONZALO_ANCESTOR_CAP", "")])).unwrap(),
            gonzalo_core::DEFAULT_ANCESTOR_CAP
        );
    }

    #[test]
    fn ancestor_cap_reads_the_variable() {
        assert_eq!(
            ancestor_cap_from_env(env(&[("GONZALO_ANCESTOR_CAP", "8")])).unwrap(),
            8
        );
    }

    #[test]
    fn ancestor_cap_rejects_zero_and_garbage() {
        let zero = ancestor_cap_from_env(env(&[("GONZALO_ANCESTOR_CAP", "0")])).unwrap_err();
        assert!(zero.contains("GONZALO_ANCESTOR_CAP"), "{zero}");
        let garbage =
            ancestor_cap_from_env(env(&[("GONZALO_ANCESTOR_CAP", "lots")])).unwrap_err();
        assert!(garbage.contains("GONZALO_ANCESTOR_CAP"), "{garbage}");
        assert!(ancestor_cap_from_env(env(&[("GONZALO_ANCESTOR_CAP", "-1")])).is_err());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p gonzalo-server --lib config::tests`
Expected: compile error `cannot find function `ancestor_cap_from_env``.

- [ ] **Step 3: Implement the parser**

In `crates/gonzalo-server/src/config.rs`, insert after the closing brace of `impl StoreConfig` (line 56):

```rust
/// Resolve the backing store's ancestor cap from `GONZALO_ANCESTOR_CAP`
/// (gonzalo#203, spec §3.9), via the environment accessor `get`.
///
/// Unset or empty → `gonzalo_core::DEFAULT_ANCESTOR_CAP`. Zero or a
/// non-number is an error, so the daemon fails fast with a clear message
/// instead of silently running at the default — the same policy as
/// [`StoreConfig::from_env`].
pub fn ancestor_cap_from_env(get: impl Fn(&str) -> Option<String>) -> Result<usize, String> {
    let Some(raw) = get("GONZALO_ANCESTOR_CAP").filter(|s| !s.is_empty()) else {
        return Ok(gonzalo_core::DEFAULT_ANCESTOR_CAP);
    };
    let cap: usize = raw
        .parse()
        .map_err(|e| format!("GONZALO_ANCESTOR_CAP must be a positive integer: {e}"))?;
    gonzalo_core::validate_ancestor_cap(cap).map_err(|e| format!("GONZALO_ANCESTOR_CAP: {e}"))
}
```

In `crates/gonzalo-server/src/lib.rs`, replace line 12:

```rust
pub use config::{StoreConfig, ancestor_cap_from_env};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p gonzalo-server --lib config::tests`
Expected: PASS, 9 tests.

- [ ] **Step 5: Wire it into `gonzalod`**

In `crates/gonzalo-server/src/bin/gonzalod.rs`, replace the doc list lines 4-8 (the store variables) with:

```rust
//! - `GONZALO_STORE`     — substrate: `fs` (default) or `s3`
//! - `GONZALO_ROOT`      — fs store root directory (default `./gonzalo-data`)
//! - `GONZALO_S3_BUCKET` — s3 bucket (required when `GONZALO_STORE=s3`)
//! - `GONZALO_S3_ENDPOINT` — s3 endpoint for MinIO/Garage (optional)
//! - `GONZALO_S3_REGION` — s3 region override (optional)
//! - `GONZALO_ANCESTOR_CAP` — revisions kept in each record's `ancestors` list
//!   (default 32, at least 1). A smaller cap turns some sync fast-forwards
//!   into merges or conflicts; it never produces a wrong winner (gonzalo#203).
```

Replace the imports at lines 24-25:

```rust
use gonzalo_core::{BlobStore, Store};
use gonzalo_server::{Auth, Service, StoreConfig, ancestor_cap_from_env, serve_grpc, serve_http};
```

Replace lines 45-69 (from `let config = StoreConfig::from_env(...)` through the end of the `match`) with:

```rust
    let config = StoreConfig::from_env(|k| std::env::var(k).ok())?;
    // Applied to the backing store, which folds ancestors inside its own OCC
    // critical section (gonzalo#203).
    let ancestor_cap = ancestor_cap_from_env(|k| std::env::var(k).ok())?;
    let (store, blobs, graph_root): (
        Arc<dyn Store>,
        Arc<dyn BlobStore>,
        Option<std::path::PathBuf>,
    ) = match &config {
        StoreConfig::Fs { root } => {
            // Per-view SQLite graphs written by `gonzalo index` live under
            // `<root>/graphs` and are queried directly.
            let fs = Arc::new(FsStore::new(root).with_ancestor_cap(ancestor_cap)?);
            let graphs = std::path::Path::new(root).join("graphs");
            (fs.clone(), fs, Some(graphs))
        }
        StoreConfig::S3 {
            bucket,
            endpoint,
            region,
        } => {
            // No local SQLite graph cache under S3: views assemble from the
            // manifest + content-addressed slices (blobs) on demand.
            let s3 = Arc::new(
                S3Store::connect(bucket.clone(), endpoint.clone(), region.clone())
                    .await
                    .with_ancestor_cap(ancestor_cap)?,
            );
            (s3.clone(), s3, None)
        }
    };
```

Replace the startup line (lines 90-93) with:

```rust
    eprintln!(
        "gonzalod: store {substrate}, ancestor cap {ancestor_cap}, HTTP on {http_addr}, gRPC on {grpc_addr}, auth {}",
        if auth_on { "on" } else { "off" }
    );
```

- [ ] **Step 6: Build and smoke-test**

Run: `cargo build -p gonzalo-server --bin gonzalod`
Expected: PASS.

Run: `GONZALO_ANCESTOR_CAP=0 GONZALO_ROOT=$(mktemp -d) GONZALO_HTTP_ADDR=127.0.0.1:0 GONZALO_GRPC_ADDR=127.0.0.1:0 ./target/debug/gonzalod`
Expected: exits non-zero immediately, printing `Error: "GONZALO_ANCESTOR_CAP: backend error: ancestor cap must be at least 1"`.

Run: `GONZALO_ANCESTOR_CAP=8 GONZALO_ROOT=$(mktemp -d) GONZALO_HTTP_ADDR=127.0.0.1:0 GONZALO_GRPC_ADDR=127.0.0.1:0 timeout 2 ./target/debug/gonzalod`
Expected: prints `gonzalod: store fs(...), ancestor cap 8, HTTP on 127.0.0.1:0, gRPC on 127.0.0.1:0, auth off`, then `timeout` stops it (exit 124).

- [ ] **Step 7: Commit**

```bash
git add crates/gonzalo-server/src/config.rs crates/gonzalo-server/src/lib.rs crates/gonzalo-server/src/bin/gonzalod.rs
git commit -m "feat(gonzalod): GONZALO_ANCESTOR_CAP for the backing store (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 7: Fold tombstone cases into `run_store_conformance`

**Files:**
- Modify: `crates/gonzalo-core/src/conformance.rs:1-3` (module doc), `run_store_conformance` (`:30-47` at `e155d25`)
- Modify: `crates/gonzalo-integration-tests/tests/server_store_conformance.rs` (remove two Task 5 tests)
- Modify: `crates/gonzalo-store-fs/tests/conformance.rs` (remove `fs_store_passes_tombstone_conformance_default_cap`, slice 2)
- Modify: `crates/gonzalo-store-git/tests/conformance.rs` (remove `git_store_passes_tombstone_conformance_default_cap`, slice 2)
- Modify: `crates/gonzalo-store-s3/tests/integration.rs` (remove `s3_store_passes_tombstone_conformance_at_default_cap`, slice 3)

**Interfaces:**
- Consumes: `run_tombstone_conformance<S, F, Fut>(factory: F, cap: usize)` with `F: Fn() -> Fut`. Passing `&factory` works because `&F: Fn() -> Fut` whenever `F: Fn() -> Fut` (std blanket impl); `S` and `Fut` infer unchanged.
- Produces: `run_store_conformance(factory)` also runs every tombstone case at `DEFAULT_ANCESTOR_CAP`. Its factory must build fresh, empty stores at the default cap. All four in-workspace callers already do: fs `FsStore::new(fresh_root())`, git `fresh_store()`, s3 `fresh_bucket_store`, server `fresh_*_store(DEFAULT_ANCESTOR_CAP)`.

- [ ] **Step 1: Fold the call in**

In `crates/gonzalo-core/src/conformance.rs`, replace the module doc (lines 1-3):

```rust
//! A reusable conformance suite every `Store` impl must pass. Substrate
//! crates call `run_store_conformance(factory)` from their integration
//! tests. The factory returns a fresh, empty store per invocation, built at
//! the default ancestor cap. `run_store_conformance` includes every tombstone
//! case (gonzalo#203) at that cap. Stores built with a smaller cap also call
//! `run_tombstone_conformance(factory, cap)` directly.
```

In `run_store_conformance`, keep every existing case call exactly as slice 1 left it, update the doc comment, and append the fold as the last statement:

```rust
/// Run the full suite against a store produced by `factory`, including the
/// tombstone cases at [`DEFAULT_ANCESTOR_CAP`](crate::DEFAULT_ANCESTOR_CAP).
/// `factory` must build fresh, empty stores at the default cap.
pub async fn run_store_conformance<S, F, Fut>(factory: F)
where
    S: Store,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = S>,
{
    // ... the existing case calls, unchanged ...

    // Every store gets the tombstone cases, so a new substrate cannot pass
    // conformance without replicated deletion (gonzalo#203).
    run_tombstone_conformance(&factory, crate::DEFAULT_ANCESTOR_CAP).await;
}
```

- [ ] **Step 2: Remove the redundant default-cap tests**

Delete these whole `#[tokio::test]` functions (attribute through closing brace):

- `crates/gonzalo-store-fs/tests/conformance.rs`: `fs_store_passes_tombstone_conformance_default_cap`
- `crates/gonzalo-store-git/tests/conformance.rs`: `git_store_passes_tombstone_conformance_default_cap`
- `crates/gonzalo-store-s3/tests/integration.rs`: `s3_store_passes_tombstone_conformance_at_default_cap`
- `crates/gonzalo-integration-tests/tests/server_store_conformance.rs`: `http_server_store_passes_tombstone_conformance` and `grpc_server_store_passes_tombstone_conformance`, the two without the `_small_cap` suffix.

Keep `fs_store_passes_tombstone_conformance_small_cap`, `git_store_passes_tombstone_conformance_small_cap`, `s3_store_passes_tombstone_conformance_at_small_cap`, and the two server `_small_cap` tests. Then drop `DEFAULT_ANCESTOR_CAP` from any of those files' `use` lists if nothing else in the file still uses it (clippy's `unused_imports` flags it).

- [ ] **Step 3: Verify no default-cap call remains outside core**

Run: `rg -n -U "run_tombstone_conformance\([\s\S]*?(DEFAULT_ANCESTOR_CAP|,\s*32)\s*,?\s*\)" crates --glob '!crates/gonzalo-core/**'`
Expected: no output.

Run: `rg -n "fn \w+tombstone_conformance\w*" crates --glob '!crates/gonzalo-core/**'`
Expected: exactly five functions, all ending in `small_cap`.

- [ ] **Step 4: Run the affected suites, one at a time**

Run: `cargo test -p gonzalo-core --features conformance`
Expected: PASS (the `MemStore` self-test now also runs the tombstone cases through `run_store_conformance`).

Run: `cargo test -p gonzalo-store-fs --test conformance`
Expected: PASS, 2 tests.

Run: `cargo test -p gonzalo-store-git --test conformance`
Expected: PASS, 2 tests.

Run: `cargo test -p gonzalo-integration-tests --test server_store_conformance`
Expected: PASS, 9 tests.

Run: `cargo test -p gonzalo-store-s3 --test integration`
Expected: PASS. Without the RustFS test environment, the live tests print `skipping:` and return early; that is expected locally.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-core/src/conformance.rs crates/gonzalo-integration-tests/tests/server_store_conformance.rs crates/gonzalo-store-fs/tests/conformance.rs crates/gonzalo-store-git/tests/conformance.rs crates/gonzalo-store-s3/tests/integration.rs
git commit -m "test(core): run tombstone cases inside run_store_conformance (#203)

Every store now gets the tombstone cases at the default cap. Explicit
default-cap tests are removed; small-cap tests stay.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 8: Full gate, push, PR

**Files:** none (verification and publishing).

- [ ] **Step 1: Format**

Run: `cargo fmt --all -- --check`
Expected: no output, exit 0. If it prints a diff, run `cargo fmt --all`, then `git commit -am "style: cargo fmt (#203)" -m "Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"`, and rerun the check.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: exit 0. The likely lint is `clippy::result_large_err` on a new `Result<_, Status>` helper in `grpc.rs`. Every helper in this plan carries the allow, so if one fires, the allow was dropped.

- [ ] **Step 3: Build**

Run: `cargo build --workspace --all-targets --all-features`
Expected: exit 0.

- [ ] **Step 4: Test**

Run: `cargo test --workspace --all-features`
Expected: exit 0, with no `FAILED` lines. Confirm the new tests ran:

- `auth::tests::is_admin_needs_wildcard_read_and_write`
- `service::tests::replication_methods_delegate_to_the_store`
- 15 new `http::tests`
- 13 new `grpc::tests::grpc_*`
- 3 new `config::tests::ancestor_cap_*`
- 8 new `gonzalo-store-server` tests
- 9 tests in `server_store_conformance`

- [ ] **Step 5: Push**

```bash
git push -u origin feat/203-tombstones-04-daemon-client
```

Expected: branch published.

- [ ] **Step 6: Open the PR**

```bash
gh pr create --base main --head feat/203-tombstones-04-daemon-client \
  --title "feat: daemon replication surface and ServerStore raw methods (#203, slice 4)" \
  --body "$(cat <<'EOF'
Part of #203

Slice 4 of the tombstones plan (`docs/superpowers/plans/2026-09-13-tombstones-04-daemon-and-client.md`).

## What

- **HTTP:** `GET /v1/raw/records/{ns}/{col}/{id}` (always `200 {"record": …|null}`), `PUT /v1/raw/records/{ns}/{col}/{id}` (`PutBody`), `GET /v1/raw/keys`, `POST /v1/purge/{ns}/{col}/{id}` with `{"expected": Revision}`.
- **gRPC:** `GetRaw`, `ListRaw`, `PutRaw` (reusing the Get/List/Put messages), `Purge` (new `PurgeRequest`/`PurgeResponse`).
- **Auth (ADR 0015):**
  - Raw reads need `read` on the namespace; unscoped `list_raw` needs `read` on `*`, like `/v1/keys`.
  - `put_raw` needs `write`. It **keeps the incoming author only for an admin** (the replication credential; open mode counts as admin) and restamps any other principal, like `put`, so authorship stays unforgeable.
  - Purge needs a full admin via the new `Principal::is_admin`, and authorizes before it parses its body (#146).
- **Deletes are stamped:** both transports call `Service::delete_as` with the authenticated principal; open mode passes `None`.
- **Put `NotFound`** now crosses the wire as `412` / `FailedPrecondition` and returns to the client as `CoreError::NotFound`, instead of an opaque `500`.
- **`ServerStore`:** real `get_raw`/`list_raw`/`put_raw`/`purge` on both transports. HTTP 404 or gRPC `Unimplemented` returns `daemon predates replication reads (gonzalo#203); upgrade gonzalod`, with **no fallback** to consumer routes (a wiremock test fails on any consumer hit).
- **`gonzalod`:** `GONZALO_ANCESTOR_CAP`, applied to the fs and s3 backing stores.
- **Conformance:** tombstone conformance runs over both transports, with a fresh daemon per factory call. `run_store_conformance` now runs the tombstone cases at the default cap; the redundant explicit default-cap tests are removed.

## Verification

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace --all-targets --all-features`, `cargo test --workspace --all-features`: all pass locally.

https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

Expected: PR URL printed. Wait for CI green, then merge (never push to `main`).

---

## Notes for the overview owner

1. **New interfaces produced here:**
   - `Principal::is_admin(&self) -> bool`
   - `pub const gonzalo_store_server::DAEMON_PREDATES_REPLICATION: &str`
   - `gonzalo_proto::http::{RawRecordBody, PurgeBody}`
   - proto `PurgeRequest`/`PurgeResponse`, with `GetRaw`/`ListRaw`/`PutRaw` reusing existing messages
   - `Service::{get_raw, list_raw, put_raw, purge, delete_as}` (`Service::delete` removed)
   - `gonzalo_server::ancestor_cap_from_env(get) -> Result<usize, String>`
2. **Wire mapping for `CoreError::NotFound` on put** (decision 7): HTTP `412` / gRPC `FailedPrecondition`, both put routes. Without it, slice 1's `put_some_over_tombstone_is_not_found` fails over the daemon. Consumer `PUT /v1/records` previously answered `500` for this error.
3. **`put_raw` authorship (coordinator decision):** ADR 0015's unforgeable-authorship invariant holds. Non-admin raw writes are restamped with the principal. Admin raw writes, including open mode, keep the incoming author, because an admin token is the replication credential. ADR 0021 (slice 7) should state that replication between daemons requires an admin token to preserve authors.
4. **`ServerStore::delete_as` ignores `author`:** attribution over the daemon always comes from the bearer token.

## Self-Review

**Spec and reconciled-contract coverage:**
- §3.6 routes, RPCs and store calls → Task 2 (HTTP, Service), Task 3 (gRPC), plus `put_raw` (reconciled change 1).
- §3.6 auth table:
  - read on ns → Task 2/3 raw-read tests;
  - unscoped admin-read → `unscoped_list_raw_requires_admin` ×2;
  - purge admin → `purge_requires_admin` ×2;
  - DELETE keeps `write` → unchanged check;
  - `put_raw` write → `put_raw_needs_write_scope_and_matching_path`, `grpc_put_raw_is_write_scoped`;
  - `put_raw` author rule (coordinator decision) → scoped writer stamped, admin keeps, open mode keeps, each on both transports.
- §3.1 author on tombstones (reconciled change 3) → `delete_stamps_the_deleter_on_the_tombstone`, `open_mode_delete_keeps_the_prior_author`, `grpc_delete_leaves_a_stamped_tombstone_for_raw_reads`.
- Reconciled `plan_put_raw` table over the wire → `put_raw_overwrites_a_tombstone_verbatim_and_none_conflicts` ×2, `put_not_found_is_412_on_both_put_routes`, `grpc_put_not_found_is_failed_precondition`. The consumer `plan_put` tombstone + `Some` → NotFound is covered by the same tests.
- §3.6 / §4.1 old-daemon upgrade error, no fallback, for all four methods → Task 4 (classify tests, wiremock `expect(0)`, service-less tonic server).
- §3.9 daemon cap → Task 6 (env only, per coordinator).
- §6.5 delete over the wire then `get_raw` → Tasks 2/3; conformance over the daemon → Task 5.
- Reconciled change 4 (fresh store per factory call) → Task 5.
- Overview "after slice 4, `run_store_conformance` calls the tombstone cases itself" → Task 7.
- Gate, push, PR with `Part of #203` → Task 8.

**Placeholder scan:** every code step carries complete code. Task 7 Step 1 shows `run_store_conformance` with an elided case list, because slice 1 owns that list and the step only appends one call after it. The exact inserted lines are given, and Step 3's `rg` checks make the removals mechanically verifiable.

**Type consistency:**
- `Service::delete_as(&RecordKey, Option<Revision>, Option<Identity>)` matches the reconciled `Store::delete_as` and both handlers.
- `Service::put_raw(Record, Option<Revision>)` matches `Store::put_raw`, both handlers and `ServerStore`.
- `RawRecordBody.record: Option<Record>` is used identically in the handler, client classifier and tests.
- `PurgeBody { expected: Revision }` is the same in the handler, tests and client.
- `PurgeRequest { namespace, collection, id, expected_json }` is the same in proto, handler, tests and client.
- `put_error`/`put_response`/`delete_outcome_parts` are used consistently in `grpc.rs`.
- `classify_put_response_for(&RecordKey, StatusCode, &str)` and `classify_raw_put_response(&RecordKey, StatusCode, &str)` match their call sites and tests.
- `put_status(Status, &RecordKey)` and `raw_put_status(Status, &RecordKey)` match theirs.
- `ancestor_cap_from_env(impl Fn(&str) -> Option<String>)` is the same in config tests and `gonzalod`.
- The test names removed in Task 7 match slices 2 and 3 verbatim.
