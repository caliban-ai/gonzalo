# ADR 0015 · Namespace-scoped daemon auth

- **Status:** accepted
- **Date:** 2026-07-05
- **Source:** [`docs/superpowers/specs/2026-07-05-namespace-scoped-daemon-auth-design.md`](../superpowers/specs/2026-07-05-namespace-scoped-daemon-auth-design.md)

## Context

The daemon (`gonzalo-server`) shipped with a single shared bearer token
(`GONZALO_TOKEN`): all-or-nothing authentication, no principals, no scoping. The
design (spec §9) always intended token auth with a `RecordKey`-namespace-scoped
permission check, "designed so finer-grained policy can slot in later without
touching the core." This ADR records that step.

Decisions in play: how principals are configured (an env-encoded blob vs a
readable file); how the new model stays backward-compatible with the single
token; whether operations without a natural namespace (graph queries by
repo/view, ticket sync) are scoped or merely authenticated; and whether the
daemon trusts the client-supplied `Meta.author` or stamps it from the
authenticated principal.

## Decision

We will replace the single-token check with a **principal model** in a pure
`auth.rs`: a token maps to a `Principal` with per-namespace `read`/`write` lists
(`"*"` = any namespace).

- **Config** — `GONZALO_AUTH_FILE` points to a TOML file of principals; if unset
  but `GONZALO_TOKEN` is set, that becomes a single admin principal (**back-compat
  preserved**); if neither is set, auth is disabled (open), matching local mode.
- **Enforcement** — both transports authenticate at the edge (token → principal,
  else 401/unauthenticated) and authorize in each handler, where the target
  namespace is known: `get`/`list` need `read`, `put`/`ticket_sync` need `write`,
  graph queries need `read` on the `repo` namespace (which is the manifest's
  namespace). `list` with no namespace requires an admin (`read` on `*`). Denied →
  403/permission_denied.
- **Non-CRUD ops are scoped, not just authenticated** — graph queries map to the
  repo namespace and ticket sync to the `tickets` namespace, both natural keys.
- **Author is stamped** — every authenticated write overwrites `Meta.author`
  with the authenticated principal, so authorship cannot be forged by a client.
  Open (disabled-auth) mode has no identity to stamp and stays a transparent
  store.

Scope is namespace-level read/write only; RBAC roles and a policy engine remain
future work.

## Consequences

- **Positive:** multi-tenant namespace isolation over both transports; existing
  single-token and open (local) deployments keep working unchanged; unforgeable
  authorship; the check lives in a pure, well-tested module.
- **Negative:** authorization is threaded through each handler (the namespace
  lives in the payload, not the headers); no sub-namespace (collection/id)
  granularity yet; tokens are static operator config with no rotation tooling.
- **Revisit if:** we need finer-grained (collection/record) permissions, roles,
  token rotation/issuance, or a real policy engine — the principal seam is where
  that slots in.
