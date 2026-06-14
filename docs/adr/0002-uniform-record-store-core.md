# ADR 0002 · Uniform `Record` + generic `Store` core (Approach A)

- **Status:** accepted
- **Date:** 2026-06-13

## Context

Gonzalo must persist several caliban data types — memory tiers, auto-memory
topics, sessions, checkpoints — across several substrates (filesystem, git, S3,
remote daemon), with versioning, optimistic concurrency, conflict surfacing,
and sync. Three shapes were considered:

- **(A)** One uniform persisted unit (`Record`) and a single generic `Store`
  trait; caliban's types become typed views layered on top.
- **(B)** A typed store per domain type — which re-implements
  versioning / conflict / sync for every *type × substrate* combination
  (combinatorial duplication).
- **(C)** A schemaless JSON-document store — which discards the type safety that
  motivates writing the system in Rust.

## Decision

Adopt **Approach A**. `gonzalo-core` defines one `Record` (key, kind, revision,
parent, body, meta, links) and a generic `Store` trait over it. Substrates
implement only the generic `Store` and never know about caliban's types.
Caliban's types live as typed views in `gonzalo-domain`, mapped to/from `Record`
via serde.

The hard parts — versioning, optimistic concurrency, conflict surfacing
(ADR 0005), and sync — are written **once** in the core, independent of both
substrate and domain type.

## Consequences

- **Positive:** Versioning / conflict / sync logic exists once, not per
  type × substrate. New substrates and new domain types are independent axes —
  adding one does not touch the other. The vector and graph layers key off the
  same `RecordKey`, so their queries return first-class records.
- **Negative:** Everything funnels through one `Record` shape; a domain type
  that fits the model poorly must still be marshalled into it. An extra mapping
  layer (domain view ↔ `Record`) sits between caliban and storage.
- **Revisit if:** a domain type cannot be reasonably expressed as a `Record`,
  or the generic core blocks a substrate-specific optimization that materially
  matters.
