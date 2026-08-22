# Changelog

All notable changes to gonzalo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, the minor version is bumped for new features and
the patch version for fixes.

## [Unreleased]

### Fixed

- **`impact` no longer merges unrelated code through shared identifiers** (#207).
  The closure walked the name-matched caller graph, so one hop into a name with
  several definitions absorbed every subgraph sharing that identifier. The walk
  now keys nodes on `(name, defining path)` and consults the resolver for every
  edge: an `Ambiguous` reference is counted and dropped rather than traversed.

  On the gonzalo view, seeded at `build_rust`: **356 → 178** reached names, with
  10 ambiguous edges reported rather than followed. Seeds that were already sharp
  are unchanged (`resolve_references_to`: 5 → 5).

  The result is now a report rather than a name list — every node carries the path
  defining it, `ambiguous_edges` says how many edges could not be attributed (so a
  non-zero count means the true set may be larger), and `truncated` reports a walk
  stopped by the new optional `max_depth`. The daemon's HTTP and gRPC transports
  keep their existing name-list shape and so get the precision fix without the
  report fields.

  Of the remaining 178, 17 are still provably false and trace to a single
  `UniqueGlobal` over-attribution — std's `Iterator::chain` resolving to a
  same-named test fixture. That is a distinct defect, filed as #223.

- **An incremental re-index now prunes paths a laxer run admitted** (#209
  follow-up). The filter added in #218 only applied to newly walked or changed
  files, so an *existing* view kept its vendored bundles forever: a bundle never
  changes, so it never appears in the git diff and was never reconsidered — and
  once a base commit is recorded there is no full walk to clean it up. Upgrading
  therefore fixed new views only, which is the case least in need of fixing.

  The carried-forward set is now re-checked against the current rules, including
  `.gitignore` — necessary because `docs/guide/book/` is build output excluded by
  ignore rules rather than by any directory-name rule, so a path-only prune left
  it behind.

  Re-indexing the existing `caliban-ai/caliban` view: 17 162 symbols → **8 549**,
  with vendored symbols going 8 618 → **0** and the largest file becoming
  `caliban/src/tui/events.rs` (142) instead of a copy of `mermaid.min.js`.

- **An unknown `repo`/`view_id` is now an error, not an empty result** (#210).
  Every graph query returned `[]` with `isError: false` when the selector named
  no indexed view, so a one-character typo in `view_id` was indistinguishable
  from a symbol that genuinely is not there — an agent read it as "nothing calls
  this" and reported a wrong answer as fact. `Service::view` now fails with
  `NotFound`, and the MCP layer turns that into a tool error naming the
  unresolved selector *and* listing the views that do exist, so a caller can
  correct itself in one round trip. A real miss inside a real view still returns
  `[]`, so the two cases are finally distinguishable.

  `diff` gets the same check on both `view_a` and `view_b`.

### Added

- **A guide chapter for the MCP server** (#208) — `docs/guide/src/mcp.md`, covering
  install → index → register → verify → keep fresh, a tool reference grouped by the
  question each tool answers, and the capability boundaries. It leads with the thing
  nothing in the repo stated: the server only *reads*, `gonzalo index` writes, so an
  unindexed setup answers every query forever with no indication why. It also records
  the traps found while wiring the server up for real — `GONZALO_ROOT` not expanding
  `~` (#211), `~/.cargo/bin` missing from the non-interactive shells an MCP client
  spawns, and needing to reconnect the server to pick up a newly installed binary —
  plus a troubleshooting table keyed by symptom.

- **`views` discovery tool and a view count in `status`** (#210). `views` lists
  every indexed `(repo, view_id)` with its file count and the commit it was
  indexed at, which makes the server self-describing rather than dependent on
  out-of-band documentation; comparing `base_commit` against the checkout's HEAD
  also surfaces a stale view, the quieter form of the same problem. `status` now
  reports how many views are indexed, so the natural health-check call actually
  detects a server pointed at an empty or wrong store. The `repo`/`view_id`
  schema descriptions now say they must match an indexed view and point at
  `views`.

- **Calls inside Rust macro arguments are now recorded as references** (#216).
  Macro arguments parse as a `token_tree` of raw tokens rather than expressions,
  so `assert_eq!(f(), 1)` contained no `call_expression` and the call to `f` was
  never seen. Because assertions are where much of a codebase is exercised, this
  silently removed a large share of the call graph: `callers`, `callees`,
  `impact` and `top by=fan_in` all undercounted, and `unreferenced` reported
  live functions as dead.

  Re-indexing gonzalo itself, with an identical file set (1 852 symbols both
  runs), references go from 10 247 to **12 196 — +1 949 edges, +19.0%**.
  `Language::from_extension`, the symbol that exposed the bug, goes from 0
  recorded references to 28.

  Detection is token-level: an identifier whose immediate next sibling is a
  parenthesised token tree. A nested macro has a `!` between the two and is
  excluded. It is deliberately over- rather than under-inclusive — a tuple-struct
  pattern like `Some(_)` reads as a call — which matches a graph that already
  records constructors and enum variants as calls.

  The other 17 grammars were audited for the same opaque-node hole. Only C/C++
  has one: a `#define` body is a single opaque `preproc_arg` token with no child
  nodes to read. It is left in place and pinned by a test so the gap is
  discoverable rather than silent.

- **The indexer no longer walks vendored bundles or gitignored build output**
  (#209). `is_indexable` skipped only `target`, `.git`, and dotted components, so
  half of a real repo's graph was not that repo's code. Membership now lives in
  one place (`IndexFilter`), shared by the full walk and the git-incremental
  driver so they cannot disagree: dependency/output directories (`node_modules`,
  `vendor`, `dist`, `build`, `site-packages`, `third_party`) and generated files
  (`*.min.js`, `*.min.css`, `*.bundle.js`, `*-lock.json`) are dropped on both
  paths, and the full walk additionally honours `.gitignore`.

  Re-indexing `caliban-ai/caliban` drops it from 16 986 symbols to 8 501 (-50.0%)
  with **zero** symbols from `book/**` or any `*.min.js`; the largest file in the
  view is now `caliban/src/tui/events.rs` (142 symbols) rather than a 4 231-symbol
  copy of `mermaid.min.js`. This also removes a reproducibility hole — indexing
  gitignored output made the graph depend on whether anyone had run a build.

  `gonzalo index` now reports what it excluded (`ignored: N files, M dirs not
  descended`), and `--include <path>` re-admits a vendored path that a built-in
  rule would drop. `--include` deliberately cannot override `.gitignore`, so no
  flag can make a view irreproducible.

### Added

- **Aggregate code-graph queries** (#214) — three MCP tools that answer questions
  about a view rather than about a symbol name the caller already has:
  `overview` (file/symbol/reference counts, a breakdown by kind and language, and
  the largest files), `top` (rank by `fan_in`, `fan_out`, or `definitions` — a
  `definitions` score above 1 marks an ambiguous name), and `list` (enumerate
  symbols filtered by `path_prefix`, `kind`, and `name_contains`). Backed by new
  default `GraphStore` methods, so every store implementation inherits them.
  Results are bounded and report `total` + `truncated` rather than silently
  cutting.
- **`unreferenced` dead-code candidates** (#214) — a fourth aggregate tool
  listing symbols with no inbound reference, filtered by the same
  `path_prefix`/`kind`/`name_contains` and bounded the same way. `exclude_tests`
  (default on) drops members of a `mod tests`/`mod test` block by line range and
  anything under a `tests/` directory; on gonzalo itself that is the difference
  between 515 hits and 40. Deliberately errs toward silence — a reference from
  anywhere counts, including from tests and from the symbol itself. Its false
  positives are documented in the tool description, the rustdoc, and a pinned
  test: calls inside macro arguments are not recorded at all (`assert_eq!(f(),
  1)` registers nothing), and a function passed as a value is a path expression
  rather than a call, so both look uncalled.

## [0.4.0] - 2026-08-01

The remote-parity & backend-qualification release. Deletion and blobs — the two
gaps that kept the daemon substrate behind `fs`/`s3` — close, so a daemon-backed
consumer now gets the full `Store` + `BlobStore` surface. Alongside them, an HA
soak harness doubles as a conditional-write qualifier for S3 backends, and its
first finding disqualifies Garage outright.

### Added

- **`Store::delete`** (#183) — OCC-aware record deletion across every substrate
  and the daemon, propagated by `Sync`. Local-only semantics; see ADR 0018.
  (#185)
- **Blobs over the daemon** (#184) — the content-addressed `BlobStore` is exposed
  on `gonzalo-server` (HTTP `GET|PUT|DELETE /v1/blobs/{hash}`, `GET /v1/blobs`,
  plus gRPC), and `ServerStore` implements `BlobStore` over both transports, so a
  daemon-backed consumer gets the full `Store` + `BlobStore` surface. Blobs
  previously worked only on `fs`/`s3`. Adds the `GONZALO_MAX_BLOB_SIZE` daemon
  knob (default 64 MiB). (#192)

### Changed

- **S3 backends are now qualified, and Garage is not among them** (#52) — atomic
  `If-Match` is a hard requirement for any S3-compatible backend. Garage does not
  provide it: gonzalo's conditional-write conformance case expects exactly 1 of 8
  concurrent racers to commit, and Garage let 8/8 through on v1.0.1 and 3–8
  through non-deterministically on v2.1.0 — the signature of a check-then-set,
  not an atomic CAS. **Deployments running gonzalo over Garage can silently lose
  concurrent writes.** RustFS (Apache-2.0) is the qualified backend; MinIO passes
  the qualifier but is rejected on project sustainability. See ADR 0019. (#186,
  #205)

Testing: an HA soak harness for stateless `gonzalod` replicas over an
S3-compatible store — backend-agnostic, doubling as the conditional-write
qualifier above (#52, #186); cross-crate integration tests extracted into
`gonzalo-integration-tests` (#190, #191).

Project: a crates.io publishing pipeline for the workspace, triggered on `v*`
tags (#187, #189).

Docs: competitor capability inventories and parity-gap matrices for mem0, Zep,
and Letta under `docs/evaluation/` (#193); ADR 0019 recording the S3 backend
qualification (#205).

## [0.3.0] - 2026-07-11

The hardening & language-breadth release. A broad correctness and robustness
sweep — a 20-finding QA pass turned into fixes across the core merge/OCC model,
every storage substrate, the daemon, the ticket connectors, and the
knowledge/vector layer — lands alongside eight new code-graph grammars that
take language coverage from nine to **seventeen**.

### Added

- **Language breadth for the code graph** — grammars for **Ruby, PHP, Bash**
  (#87), **Kotlin** (#87), **Swift** (#87), **Lua** (#87), **Scala** (#87), and
  **Elixir** (#87). Elixir is homoiconic (`def`/`defp`/`defmacro`/`defmodule`
  parse as ordinary `call` nodes), so it uses a value-based `walk()` dispatch on
  the call target's text rather than a node-kind mapping. Coverage is now 17
  languages. (#126, #127, #128, #129, #130, #181)

### Changed

- **Ticket `RecordKey`s are board-scoped** — a board-scoped source folds its
  connection/board discriminator into the key, so the same issue imported from
  two boards no longer collides onto one thrashing record. Stored keys for board
  sources change shape and re-import on the next sync. (#159)

### Fixed

- **core** — record-key encoding is now a reversible, injective percent-style
  codec, so distinct keys can never collide onto one physical path (silent
  cross-key overwrite / OCC bypass); append-only merge preserves blank and
  legitimately-repeated committed lines instead of stripping/de-duping them, and
  the `Derived`/gc semantics are corrected. (#131, #133)
- **storage substrates** — the git substrate locks the `put` critical section
  (no lost updates under concurrent writers) and detects non-fast-forward push
  rejection; filesystem writes fsync the temp file and parent directory for
  crash durability; S3 list-pagination terminates when the continuation token is
  absent; graph-sqlite view-db paths use the injective encoder. (#132, #134,
  #144, #145)
- **daemon** — authorization runs before request deserialization, internal
  backend errors are returned opaquely (no path/bucket/SQLite leakage), the PUT
  record route validates its URL path against the body key, and the remote
  client surfaces daemon 403/413 responses instead of masking them as a decode
  error. (#146, #147)
- **code graph** — JS/TS arrow-function and function-expression bindings are
  extracted, PHP method and static calls are recorded, and Swift/Kotlin
  `struct`/`enum`/`interface` declarations get their correct `SymbolKind`. (#136)
- **knowledge / vector / domain** — a corrupt knowledge-bearing body surfaces an
  ingest error (rather than silently not indexing) and removed records are
  de-indexed; non-finite vectors score `0.0` and rank deterministically; the
  domain codec rejects a `Body::Blob` instead of misparsing its content hash.
  (#139, #149, #154)
- **cli** — `get`/`ticket get` exit non-zero (message on stderr) when a record
  is absent, `index` advances the persistent SQLite graph only after the
  manifest commits, and `--gc` is honored under `--watch`. (#152)
- **ticket connectors** — Jira routes a `Canceled` move to a won't-do status
  rather than Done; a closed GitLab issue is terminal and non-terminal moves no
  longer report a false success; Linear fails a mutation that returns
  `success: false`; the GitHub REST connector follows `Link` pagination so all
  issues import, not just the first 100. (#138, #140, #141, #142)

Testing: de-flaked `gonzalo-parse`'s hung-worker timeout test, whose 300 ms
budget false-timed-out the healthy recovery parse under heavy build load. (#178)

## [0.2.0] - 2026-07-06

The code-graph release. Gonzalo grows a full **code-graph capability** —
tree-sitter parsing across nine languages, content-addressed slices with
two-level keying, a persistent SQLite `GraphStore`, and structural queries over
both the daemon and an MCP server — alongside real semantic search (a local
Candle embedder feeding an approximate ANN index over per-kind-chunked
knowledge), namespace-scoped daemon auth, and correct content-aware 3-way merges
on both store-sync and git pull. Distributed as the `gonzalod` container image.

### Added

- **Code-graph capability** (EPIC A–K): tree-sitter parsing into path-agnostic,
  content-addressed slices with two-level keying (content+grammar hash for
  storage, per-view manifest for resolution); a persistent `SqliteGraphStore`;
  assembly-time name resolution; and structural queries — definitions,
  references, callers, callees, transitive impact, and cross-view diff — served
  over the daemon and a dedicated **code-graph MCP server**. Parsing is
  crash-isolated behind a worker-subprocess `ParserPool`. (ADR 0012;
  #48, #50, #54, #56, #61, #64, #66, #70, #71, #74, #77, #88, #89, #90, #10, #30)
- **Language breadth for the code graph** — a `Language` dispatch with grammars
  for Rust, Python, JavaScript/TypeScript/TSX, Go, Java, C#, C, and C++. (#79,
  #81, #83, #84, #85, #86)
- **CLI indexing** (`gonzalo index`): index a source tree into a code-graph view,
  git-diff-driven incremental re-sync, a `--watch` file-watcher for live
  re-index, and opt-in mark-sweep GC of unreferenced slices. (#74, #93, #94,
  #100, #104)
- **Knowledge store** (`gonzalo-knowledge`): a "what do we know about X" surface
  composing `Store` + `VectorIndex` + `Embedder`, a **vector⋈graph** join
  (semantically similar *and* structurally near), and **per-kind chunking** so
  long records retrieve at turn/section granularity. (ADR 0011; #30, #29)
- **Real semantic vector search**: `gonzalo-embed` — a local CPU sentence
  embedder (Candle + all-MiniLM-L6-v2) — and `HnswVectorIndex`, an approximate
  ANN backend, behind the existing `Embedder`/`VectorIndex` traits.
  (ADR 0013, ADR 0014; #97, #9)
- **Daemon substrate selection & health**: env-driven `fs|s3` store selection
  with a native S3 `BlobStore`, and unauthenticated `/healthz` + `/readyz`
  probes for k8s. (#62, #63)
- **Namespace-scoped daemon auth**: a token→principal model with per-namespace
  read/write scoping enforced on both transports, plus unforgeable author
  stamping. (ADR 0015; #11)
- **`gonzalod` container image** — the release artifact, published on a `v*` tag.
  (#51, #65)

### Changed

- **Store sync — true 3-way merge**: `AncestryStore` retains each version's body
  by revision hash so `sync` can merge divergent structured records against their
  real common ancestor instead of an empty base. (ADR 0016; #2)
- **Git pull — content-aware non-fast-forward merge**: a diverged pull now
  reconciles per-record through gonzalo's class-aware `merge()` into a two-parent
  merge commit, surfacing unresolved records instead of erroring. (ADR 0017; #7)
- **S3 native conditional writes**: `If-Match`/`If-None-Match` close the
  optimistic-concurrency TOCTOU window in the S3 substrate. (#5)

### Fixed

- CI: serialize GitHub Pages deploys with a concurrency group. (#107)

### Internal

- ADRs 0010–0017 added (ticket capability layer, two-level code-graph keying,
  local embedder, ANN backend, namespace auth, stored-ancestry 3-way merge,
  content-aware non-FF pull).
- Docs: an mdBook guide publishing the ADR log, changelog, and a synthesized
  **Guiding Principles & Invariants** page; README status badges. (#103, #38)

## [0.1.0] - 2026-07-03

The initial development line — a generic, versioned, conflict-aware persistence
layer for [caliban](https://github.com/caliban-ai/caliban), built milestone by
milestone (M1–M6).

### Added

- **Record/Store core** (M1): `gonzalo-core` — one uniform `Record` model and a
  generic `Store` trait, with revisions, optimistic-concurrency `parent`
  tracking, `PutResult::Conflict`, per-`RecordKind` merge, and a feature-gated
  substrate **conformance suite**. No I/O in the core. (ADR 0002, ADR 0005,
  ADR 0006)
- **Filesystem substrate + domain + facade** (M1): `gonzalo-store-fs` (mirrors
  caliban's on-disk layout, the zero-dependency default), `gonzalo-domain`
  (typed `MemoryTier`/`Topic`/`Session`/`Checkpoint` views), and the `gonzalo`
  facade. (ADR 0004, ADR 0008, ADR 0009)
- **Git & S3 substrates + Sync** (M2): `gonzalo-store-git` (commit-per-write,
  fast-forward pull/push) and `gonzalo-store-s3` (S3-compatible object store),
  plus the `Sync` engine reusing the core conflict/merge machinery. (ADR 0004,
  ADR 0005)
- **Daemon + remote substrate** (M3): `gonzalo-proto` (one canonical schema),
  `gonzalo-server` (`gonzalod`) serving the store over **both** gRPC (tonic) and
  HTTP/JSON (axum) on one core service layer with optional bearer auth, and
  `gonzalo-store-server` as the client substrate. (ADR 0007)
- **Vector layer** (M4): `gonzalo-vector` — `Embedder` + `VectorIndex` traits
  with a caller-delegating default embedder and an exact in-memory cosine index.
  (ADR 0008)
- **Code-graph layer** (M5): `gonzalo-graph` — a tree-sitter Rust symbol/ref
  index (`build_rust`) behind a `GraphStore` trait. (ADR 0008)
- **Admin CLI** (M6): `gonzalo-cli` (`gonzalo`) — `list`, `get`, `status`,
  `migrate`, `sync`.

### Internal

- Project: established `docs/adr/` (MADR-lite) with the initial retrospective
  ADRs 0001–0009; added CI (fmt/clippy/build/test), a line-coverage gate, the
  Kanban label taxonomy, and board/triage automation.

[Unreleased]: https://github.com/caliban-ai/gonzalo/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/caliban-ai/gonzalo/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/caliban-ai/gonzalo/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/caliban-ai/gonzalo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/caliban-ai/gonzalo/releases/tag/v0.1.0
