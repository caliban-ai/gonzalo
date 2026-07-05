# Namespace-scoped daemon auth

- **Ticket:** gonzalo#11
- **Date:** 2026-07-05
- **Status:** Accepted
- **Records:** ADR 0015
- **Refs:** `crates/gonzalo-server/src/{grpc,http,config,service}.rs`,
  `docs/superpowers/specs/2026-06-05-gonzalo-design.md` §9 (deferred auth seam)

## Problem

The daemon has only a single shared bearer token (`GONZALO_TOKEN`): if set, every
request must carry `Authorization: Bearer <token>`, all-or-nothing, with no
principals and no per-namespace scoping. The client substrate already sends the
token. Spec §9 designed for this to grow into "token-based with a
`RecordKey`-namespace-scoped permission check" — this implements that.

## Design

### `auth.rs` — pure, unit-testable core

```rust
pub enum Access { Read, Write }

pub struct Principal { name: String, read: Vec<String>, write: Vec<String> } // "*" = all
impl Principal {
    pub fn name(&self) -> &str;
    pub fn allows(&self, access: Access, namespace: &str) -> bool; // "*" or exact match
}

pub enum Auth { Disabled, Enabled(HashMap<String, Principal>) } // token -> principal
impl Auth {
    /// Disabled -> an implicit full-access admin; Enabled -> token lookup.
    pub fn authenticate(&self, token: Option<&str>) -> Option<Principal>;
    /// Parse the TOML principals file (pure; the binary does the file IO).
    pub fn parse_toml(s: &str) -> Result<Auth, String>;
}
```

`allows` treats `"*"` in a principal's `read`/`write` list as "any namespace".
`authenticate` returns `None` only when `Enabled` and the token is missing or
unknown (the caller maps that to 401/unauthenticated); `Disabled` always returns
an implicit admin principal so handlers have a uniform `Principal` to authorize
against.

### Config resolution (mirrors `config.rs`'s env pattern)

Pure selection over an env accessor, IO done by the binary:

- `GONZALO_AUTH_FILE` set → read + `Auth::parse_toml` → `Auth::Enabled`.
- else `GONZALO_TOKEN` set → `Auth::Enabled` with one admin principal
  (`read=["*"], write=["*"]`) — **back-compat: existing single-token deployments
  are unchanged**.
- else → `Auth::Disabled` (open; preserves local/library behavior).

TOML shape:

```toml
[[principal]]
name  = "caliban"
token = "s3cret"
read  = ["memory", "sessions"]
write = ["memory", "sessions"]

[[principal]]
name  = "admin"
token = "root"
read  = ["*"]
write = ["*"]
```

### Enforcement — authenticate at the edge, authorize in handlers

Both transports **always** resolve a `Principal` (the implicit admin under
`Disabled`) and carry it in request extensions; health/readiness probes stay
exempt.

- **HTTP (axum):** an auth middleware runs unconditionally — authenticates the
  bearer token → `Principal` (or `401`), inserts it into request extensions.
  Handlers take `Extension<Principal>` and authorize before delegating.
- **gRPC (tonic):** the interceptor authenticates → inserts `Principal` into the
  request extensions (or returns `Status::unauthenticated`); each method reads it
  from `req.extensions()` and authorizes.

### Per-op namespace mapping (all natural keys)

| Op                 | Namespace                         | Access                                   |
|--------------------|-----------------------------------|------------------------------------------|
| `get`              | `key.namespace`                   | Read                                     |
| `put`              | `record.key.namespace`            | Write **+ stamp `meta.author`**          |
| `list` (with ns)   | `prefix.namespace`                | Read                                     |
| `list` (no ns)     | —                                 | requires admin (`read` on `*`)           |
| graph queries      | `repo` (= `Manifest::key` namespace) | Read                                  |
| `ticket_sync`      | `"tickets"` (ticket record namespace) | Write                                |

Denied access → `403` (HTTP) / `Status::permission_denied` (gRPC). Missing/unknown
token under `Enabled` → `401` / `Status::unauthenticated`.

**Author stamping.** On every **authenticated** write the daemon overwrites
`record.meta.author` with `Identity::new(principal.name())`, so a client cannot
forge authorship — the recorded author is exactly the authenticated principal.
Under `Disabled` (open) mode there is no authenticated identity, so the daemon
stays a transparent store and leaves `meta.author` untouched (this keeps the
`Store` conformance round-trip intact).

### Why authorize in handlers, not a blanket middleware

The permission check needs the target namespace, which lives in the request
payload (path/body/query), not just the headers. Authentication (token →
principal) is header-only and stays at the transport edge; authorization
(namespace + access) happens in each handler where the namespace is known. This
keeps `Service` unchanged and the authz logic co-located with request decoding.

## Testing

- **`auth.rs` units:** `allows` exact/wildcard/deny; `parse_toml` (valid,
  malformed, wildcard); config precedence (`GONZALO_AUTH_FILE` > `GONZALO_TOKEN`
  > disabled).
- **HTTP + gRPC transport tests:** allowed vs denied read/write across
  namespaces; admin wildcard passes everywhere; disabled-mode passthrough; a
  write's `meta.author` is stamped to the principal; `list` without a namespace
  is denied for a scoped (non-admin) principal; probes remain unauthenticated;
  a missing/bad token is 401/unauthenticated.

## Scope boundaries (YAGNI)

- Namespace-level read/write only — no collection/id granularity, no RBAC roles,
  no policy engine (spec §9's "finer-grained policy can slot in later").
- No token issuance/rotation tooling; tokens are operator-provided config.
- No per-graph-view scoping beyond the repo namespace.

## Acceptance criteria

- [ ] `auth.rs` principal/token model with namespace read/write + `"*"`.
- [ ] Config: `GONZALO_AUTH_FILE` (TOML) with `GONZALO_TOKEN` back-compat and
      open-when-unset.
- [ ] Per-namespace read/write enforced on gRPC + HTTP for record CRUD, graph
      queries (by repo), and ticket sync (by `tickets`).
- [ ] Writes stamp `meta.author` from the authenticated principal.
- [ ] Allowed/denied tests across namespaces on both transports.
- [ ] ADR 0015 records the model.
