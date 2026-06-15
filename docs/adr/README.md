# Architecture Decision Records

This directory records the architecturally significant decisions made on
**gonzalo**, in [MADR-lite](https://adr.github.io/madr/) format — the same
convention used by sibling repo [caliban](https://github.com/caliban-ai/caliban)
(and, per [caliban-ai/prospero#30](https://github.com/caliban-ai/prospero/issues/30),
soon prospero too).

An ADR captures a decision that is **architecturally significant**, **costly to
reverse**, or **constrains future work**, together with the context and the
trade-off behind it — so a reader who wasn't there understands *why*. ADRs are
an append-only log: once accepted, an ADR is not rewritten. A decision that
changes gets a *new* ADR that supersedes the old one, and the old one is marked
`superseded` with a link both ways.

## Status legend

- **proposed** — under discussion, not yet adopted
- **accepted** — adopted and in effect
- **rejected** — considered and declined (kept for the record)
- **deprecated** — no longer applies, but not replaced by a specific ADR
- **superseded** — replaced by a later ADR (linked)

## Index

| ADR | Title | Status |
|-----|-------|--------|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | accepted |
| [0002](0002-uniform-record-store-core.md) | Uniform `Record` + generic `Store` core (Approach A) | accepted |
| [0003](0003-license-agpl-3.0.md) | License: AGPL-3.0-only | accepted |
| [0004](0004-pluggable-storage-substrates.md) | Pluggable storage substrates behind one `Store` trait | accepted |
| [0005](0005-optimistic-concurrency-and-conflict-surfacing.md) | Optimistic concurrency with explicit conflict surfacing | accepted |
| [0006](0006-substrate-conformance-suite.md) | Shared substrate conformance suite | accepted |
| [0007](0007-dual-transport-daemon.md) | Dual-transport daemon: gRPC + HTTP/JSON over one schema | accepted |
| [0008](0008-capability-layers-over-core.md) | Domain, vector, and graph as capability layers over core | accepted |
| [0009](0009-workspace-layout-and-facade.md) | Workspace layout and single-facade public surface | accepted |
| [0010](0010-ticket-system-capability-layer.md) | Ticket systems as a normalized work-item capability layer | accepted |
| [0011](0011-knowledge-store-capability.md) | Knowledge store over the capability layers | accepted |

## Adding a new ADR

1. Copy [`template.md`](template.md) to `NNNN-kebab-title.md` — it carries the
   section skeleton (Status / Date / Context / Decision / Consequences) every
   record uses. The accepted ADRs remain the reference for voice and length.
2. Number it with the next zero-padded integer (`NNNN-kebab-title.md`).
3. Fill in **Status**, **Date** (`YYYY-MM-DD`), and the **Context / Decision /
   Consequences** sections. Keep it to a screen or two.
4. Add a row to the index table above.
5. If it supersedes an earlier ADR, set that ADR's status to `superseded` and
   link both ways.
