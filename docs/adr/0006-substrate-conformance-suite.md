# ADR 0006 · Shared substrate conformance suite

- **Status:** accepted
- **Date:** 2026-06-13

## Context

Four substrates (fs, git, S3, server) implement the same `Store` contract, with
subtle shared semantics: revision monotonicity, conditional-put conflict
behavior, body round-tripping, key listing. If each substrate were tested only
in isolation, they would inevitably drift in behavior and caliban could not
treat them interchangeably.

## Decision

`gonzalo-core` ships **one shared conformance test suite** that every `Store`
implementation must pass. Each substrate crate runs the suite against its own
backend (fs, git, S3, the daemon-backed server substrate). The suite is the
executable definition of what "being a `Store`" means.

## Consequences

- **Positive:** All substrates are held to one behavioral spec, so they are
  genuinely interchangeable. A new substrate's correctness bar is simply "pass
  the suite." Semantics live in one place rather than scattered across
  per-substrate tests.
- **Negative:** The suite is a coupling point — tightening it can require work
  across every substrate at once. Backends that need external services (S3, the
  daemon) require test infrastructure (wiremock / testcontainers) to run it.
- **Revisit if:** a legitimate substrate cannot satisfy a suite assertion, and
  the contract needs capability tiers rather than one flat spec.
