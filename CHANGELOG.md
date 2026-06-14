# Changelog

All notable changes to gonzalo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, the minor version is bumped for new features and
the patch version for fixes.

## [Unreleased]

The initial 0.1.0 development line — a generic, versioned, conflict-aware
persistence layer for [caliban](https://github.com/caliban-ai/caliban), built
milestone by milestone (M1–M6). Not yet tagged or published.

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

[Unreleased]: https://github.com/caliban-ai/gonzalo/commits/main
