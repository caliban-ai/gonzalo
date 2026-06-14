# ADR 0004 · Pluggable storage substrates behind one `Store` trait

- **Status:** accepted
- **Date:** 2026-06-13

## Context

Gonzalo's value proposition is taking caliban's local-first state shared "by
configuration, not code." That requires several backends — a local filesystem
(today's behavior), git (auditable shared history), S3 (large cheap blobs), and
a remote daemon (central server) — without each backend re-deriving the core
semantics, and without forcing every build to pull heavy dependencies (git2,
aws-sdk, tonic).

## Decision

Each backend is a separate crate implementing `gonzalo-core::Store`:

- `gonzalo-store-fs` `[fs]` — filesystem, mirrors caliban's on-disk layout; the
  zero-dependency reference/default.
- `gonzalo-store-git` `[git]` — commit-per-write, fast-forward pull/push.
- `gonzalo-store-s3` `[s3]` — S3-compatible object store.
- `gonzalo-store-server` `[remote]` — proxies to a remote daemon over HTTP or
  gRPC (client side of ADR 0007).

Heavy dependencies live only in their owning substrate crate and are selected
through facade Cargo features (ADR 0009), so a filesystem-only build stays lean.
Which substrate backs caliban is configuration, not API.

## Consequences

- **Positive:** Going from local to git / S3 / daemon is a config change, not a
  code change. A default build pays for nothing but `fs`. New backends slot in
  by implementing one trait and passing the conformance suite (ADR 0006).
- **Negative:** The `Store` trait is a lowest-common-denominator surface —
  substrate-specific capabilities must fit it or be abstracted away. Every
  substrate carries the cost of conforming to the full semantics (versioning,
  conflicts) even where its native model differs.
- **Revisit if:** a needed backend cannot satisfy the `Store` contract, or the
  trait starts accreting substrate-specific escape hatches.
