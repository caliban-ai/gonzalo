# Summary

[gonzalo](./introduction.md)

# Design

- [Guiding Principles & Invariants](./principles.md)

# Guides

- [The MCP server](./mcp.md)

# Changelog

- [Changelog](./changelog.md)

# Architecture Decisions

- [ADR Index](./adr/index.md)
<!-- adrs -->
  - [ADR 0001 · Record architecture decisions](./adr/0001-record-architecture-decisions.md)
  - [ADR 0002 · Uniform `Record` + generic `Store` core (Approach A)](./adr/0002-uniform-record-store-core.md)
  - [ADR 0003 · License: AGPL-3.0-only](./adr/0003-license-agpl-3.0.md)
  - [ADR 0004 · Pluggable storage substrates behind one `Store` trait](./adr/0004-pluggable-storage-substrates.md)
  - [ADR 0005 · Optimistic concurrency with explicit conflict surfacing](./adr/0005-optimistic-concurrency-and-conflict-surfacing.md)
  - [ADR 0006 · Shared substrate conformance suite](./adr/0006-substrate-conformance-suite.md)
  - [ADR 0007 · Dual-transport daemon: gRPC + HTTP/JSON over one schema](./adr/0007-dual-transport-daemon.md)
  - [ADR 0008 · Domain, vector, and graph as capability layers over core](./adr/0008-capability-layers-over-core.md)
  - [ADR 0009 · Workspace layout and single-facade public surface](./adr/0009-workspace-layout-and-facade.md)
  - [ADR 0010 · Ticket systems as a normalized work-item capability layer](./adr/0010-ticket-system-capability-layer.md)
  - [ADR 0011 · Knowledge store over the capability layers](./adr/0011-knowledge-store-capability.md)
  - [ADR 0012 · Two-level keying for the code graph](./adr/0012-code-graph-two-level-keying.md)
  - [ADR 0013 · Local Candle embedder for real semantic embeddings](./adr/0013-local-candle-embedder.md)
  - [ADR 0014 · Approximate vector index backend (hnsw_rs)](./adr/0014-approximate-vector-index-backend.md)
  - [ADR 0015 · Namespace-scoped daemon auth](./adr/0015-namespace-scoped-daemon-auth.md)
  - [ADR 0016 · 3-way merge with content-addressed stored ancestry](./adr/0016-threeway-merge-stored-ancestry.md)
  - [ADR 0017 · Non-fast-forward git pull via content-aware merge](./adr/0017-nonff-pull-content-merge.md)
  - [ADR 0018 · Record deletion and its sync semantics](./adr/0018-record-deletion-and-sync.md)
  - [ADR 0019 · Qualified S3 backend for HA: RustFS](./adr/0019-s3-backend-qualification-rustfs.md)
