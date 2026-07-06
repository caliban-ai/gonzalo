# Changelog

All notable changes to gonzalo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, the minor version is bumped for new features and
the patch version for fixes.

## [Unreleased]

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

[Unreleased]: https://github.com/caliban-ai/gonzalo/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/caliban-ai/gonzalo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/caliban-ai/gonzalo/releases/tag/v0.1.0
