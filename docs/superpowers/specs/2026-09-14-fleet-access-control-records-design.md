# Fleet access-control records

- **Status:** Approved (ADR 0022)
- **Date:** 2026-09-14
- **Issues:** gonzalo#277 (design), gonzalo#278 (implementation)
- **Consumers:** Ariel chat bridge (caliban-ai/prospero#67, ariel#15–#18); later
  the prospero API (prospero#2)
- **Source:** Ariel design spec, `docs/superpowers/specs/2026-07-03-ariel-chat-bridge-design.md`
  in the caliban-ai umbrella repo, §"Identity, RBAC & audit"

## 1. Problem

Ariel keeps all of its identity, role, channel and audit state in gonzalo and
stores nothing itself. It needs six record types. None fits an existing
`RecordKind`, and the enum is closed, so they can't be defined outside this
repo. Principle 5 allows a capability layer to register new kinds and their
merge classes, and nothing more in core. This spec fixes the kinds, their keys,
their bodies, their merge classes, and the rules the typed views enforce.

The layer is named for **fleet access control**, not for Ariel. Any fleet
surface that authenticates humans (chat, the prospero web UI, a CLI) resolves
them to the same people and roles.

## 2. Goals and non-goals

**Goals**

- Six `RecordKind`s with merge classes that stay correct under `sync` and git
  `pull`.
- Typed views in `gonzalo-domain` that make the unsafe things hard: storing a
  raw link token, redeeming a token twice, rewriting an audit entry.
- A key layout that lets daemon auth (ADR 0015, namespace-granular) separate
  the audit trail from the rest.

**Non-goals**

- **gonzalo does not enforce fleet roles.** These records are data. Ariel (and
  later prospero) evaluate them, including the two-key rule
  `effective = min(user_role, channel_ceiling)`. gonzalod's own API stays on
  ADR 0015's token `Principal`.
- No store operations in `gonzalo-domain`. The views are synchronous and pure;
  the consumer does `get` / `put`.
- No random number generation in gonzalo. The caller supplies a link token's
  secret bytes.
- No identity lookup by email or username (§5.2).
- No scoping model for memory records; that is gonzalo#200 (§8).

## 3. Namespaces and keys

Two namespaces, because ADR 0015 grants `read` / `write` per namespace only:

| Namespace | Holds | Why separate |
|---|---|---|
| `fleet` | people, identity bindings, role grants, channel configs, link tokens | the mutable access-control state |
| `fleet-audit` | audit entries | a dashboard or auditor token can read the audit trail without reading link tokens or grants, and a writer that only appends audit entries needs no write on `fleet` |

Constants in `gonzalo-domain`: `FLEET_NAMESPACE = "fleet"`,
`FLEET_AUDIT_NAMESPACE = "fleet-audit"`.

| Kind | Key |
|---|---|
| `Person` | `fleet/people/<person_id>` |
| `IdentityBinding` | `fleet/identity-bindings/<authenticator>:<subject>` |
| `RoleGrant` | `fleet/role-grants/<person_id>:<scope>` |
| `ChannelConfig` | `fleet/channels/<provider>:<channel_id>` |
| `LinkToken` | `fleet/link-tokens/<token_hash>` |
| `AuditEntry` | `fleet-audit/entries/<at_ms>-<nonce>` |

**Composite ids are built only by the views' key helpers**, never by callers.
Each component is escaped (`%` → `%25`, then `:` → `%3A`) before joining with
`:`, so a composite id is injective even when a component contains `:` (an OIDC
issuer URL does). The store's own path encoding (`gonzalo_core::paths`) is
already injective, so any resulting id string is safe on every substrate.

Segments:

- `<authenticator>`: `discord`, `slack`, `teams`, `oidc:<issuer>`,
  `other:<name>` (the issuer or name escaped as a component).
- `<scope>`: `fleet`, or `repo:<owner/name>`.
- `<person_id>`: an opaque id the minting consumer generates. It is
  **not** an email, a platform id, or a username, because those change or differ
  between platforms. The view accepts `[A-Za-z0-9_-]{1,64}` and rejects anything
  else, so a person id never needs escaping.
- `<at_ms>`: the entry's `at`, zero-padded to 13 digits, so keys sort by time
  within the same digit count. Negative `at` is rejected.
- `<nonce>`: caller-supplied, `[A-Za-z0-9_-]{1,32}`.

All timestamps in these records are **milliseconds since the Unix epoch, as
`i64`**, matching `Record.deleted_at` (ADR 0021).

## 4. Record bodies

Bodies are JSON via `RecordCodec`, one struct per kind, each exposing
`pub const KIND`.

```rust
// Named with a `Fleet` prefix so they don't collide with `ticket::Actor` at the
// `gonzalo-domain` and facade roots.
pub enum FleetRole { Viewer, Operator, Admin }      // derives Ord: Viewer < Operator < Admin
pub enum GrantScope { Fleet, Repo(String) }         // Repo holds "owner/name"

/// Who did something. Shared by grants, link tokens and audit entries.
pub enum FleetActor {
    Person(String),                                  // a person_id
    Unlinked { authenticator: Authenticator, subject: String },
    Service(String),                                 // e.g. "ariel", "ariel-cli"
}

pub struct Person {
    pub display_name: String,
    pub email: Option<String>,                       // contact only (§5.2)
}

pub enum Authenticator { Discord, Slack, Teams, Oidc { issuer: String }, Other(String) }

pub struct VerifiedEmail { pub address: String, pub verified: bool }

pub struct IdentityBinding {
    pub authenticator: Authenticator,                // stored as a one-element array; merges atomically (§6)
    pub subject: String,                             // platform user id / OIDC `sub`
    pub person: String,                              // person_id
    pub handle: Option<String>,                      // platform username snapshot
    pub email: Option<VerifiedEmail>,                // stored as a one-element array; merges atomically (§6)
    pub bound_at: i64,
    pub bound_by: BindingOrigin,                     // stored as a one-element array; merges atomically (§6)
}
pub enum BindingOrigin { LinkToken { token_hash: String }, Operator(FleetActor) }

pub struct RoleGrant {
    pub person: String,
    pub scope: GrantScope,                           // stored as a one-element array; merges atomically (§6)
    pub role: FleetRole,
    pub granted_by: FleetActor,                      // stored as a one-element array; merges atomically (§6)
    pub granted_at: i64,
}

pub struct ChannelConfig {
    pub provider: String,                            // "discord", "slack", …
    pub channel_id: String,
    pub ceiling: FleetRole,
    pub repos: Vec<String>,                          // followed repos, "owner/name"
    pub filters: BTreeMap<String, serde_json::Value>, // consumer-defined (Ariel open question 4)
}

pub struct LinkToken {
    pub token_hash: String,
    pub role: FleetRole,
    pub scope: GrantScope,
    pub person: Option<String>,                      // Some: add an account to an existing person
    pub minted_by: FleetActor,
    pub minted_at: i64,
    pub expires_at: i64,
    pub consumed: Option<Consumption>,
}
pub struct Consumption { pub person: String, pub binding: RecordKey, pub at: i64 }

pub struct AuditEntry {
    pub actor: FleetActor,
    pub action: String,                              // "spawn", "link", "grant", …
    pub target: String,                              // what it acted on
    pub at: i64,
    pub surface_ref: Option<String>,                 // e.g. chat message link
    pub result: AuditResult,
}
pub enum AuditResult { Succeeded, Denied, Failed(String) }
```

`ChannelConfig` does not derive `Eq`, because `serde_json::Value` isn't `Eq`
(the same reason as `Ticket`).

### 4.1 Where identifying data lives

| Data | Field | Rule |
|---|---|---|
| Fleet user id | `Person` key id | opaque, stable, what everything references |
| Platform user id / OIDC `sub` | `IdentityBinding.subject` (and key) | the only thing lookups use |
| Username / handle | `IdentityBinding.handle` | display snapshot, refreshed by the consumer, never a lookup key |
| Display name | `Person.display_name` | the one name shown across surfaces |
| IdP-asserted email | `IdentityBinding.email` | kept with the account that asserted it, verified flag included |
| Contact email | `Person.email` | operator-set, informational |

## 5. Rules the views enforce

### 5.1 Link tokens

- **Only the hash is stored.** `LinkSecret` wraps 32 caller-supplied random
  bytes: `LinkSecret::from_bytes([u8; 32])`, `LinkSecret::parse(&str)` (64
  lowercase hex characters), `to_hex()` for displaying to the operator once, and
  `hash()`, which returns `ContentHash::of(b"gonzalo:link-token:v1" ‖ bytes)` as
  hex. `LinkToken` has no field that can hold the secret, and `LinkToken::new`
  takes a `&LinkSecret`, storing only its hash. An unsalted fast hash is correct
  here because the input is 256 random bits, not a password.
- **Redeeming marks the token consumed; it never deletes it.**
  `LinkToken::redeem(&self, secret, person, binding, now_ms) -> Result<LinkToken, RedeemError>`
  returns the consumed view, or:
  - `WrongSecret` when `secret.hash() != token_hash`;
  - `Expired` when `now_ms >= expires_at`;
  - `AlreadyConsumed` when `consumed.is_some()`;
  - `PersonMismatch` when the token names a `person` and the redeeming person
    differs, so a token minted to add an account to one person can't attach it
    to another.

  The checks run in that order.

  The consumer writes the result with `put(record, Some(read_revision))`. Two
  concurrent redemptions of the same revision are ordinary OCC: exactly one
  `Committed`, one `Conflict`.
- **Why mark rather than delete.** ADR 0021 made deletion replicate, so the
  #277 draft's reason (deletes resurrect) no longer holds. Marking is still
  right: a tombstone has no body, so a delete would lose who redeemed the token
  and when; and because redemption checks `expires_at`, a token that reappears
  after its tombstone was collected is already expired. A consumer may delete
  a token **after** it expires, never before.
- **Under sync.** A redeemed token descends from the unredeemed revision it was
  read at, so its `ancestors` contain that revision and sync fast-forwards the
  peer (ADR 0021). The token stays consumed, with no conflict. Two peers that
  each redeem the same unredeemed revision offline diverge; `LinkToken` is
  `Opaque`, so sync reports a `SyncConflict` and writes neither side.

### 5.2 No lookup by email or username

A person gains an account only through a redeemed link token or an explicit
operator action (`BindingOrigin`). Handles are renamed and re-registered by
other people; some platforms and IdPs return unverified emails. Matching on
either would let a stranger take over a person. The views offer no helper that
resolves a person from `handle` or `email`.

### 5.3 Audit entries

- One record per entry. The consumer creates it with `put(record, None)`. A
  `Conflict` means the key already exists (a nonce collision): pick a new nonce
  and retry. An entry is never updated.
- `AuditEntry::key(&self, nonce) -> Result<RecordKey, FleetKeyError>` builds the key
  from `at` and the nonce.
- Retention is the operator's: `gonzalo reset --namespace fleet-audit` and
  `collect` apply as for any namespace, gated by daemon write access.

### 5.4 Grants and bindings

- One grant per `(person, scope)`. Changing a role is an OCC `put` on that
  record; revoking is a `delete`, which replicates as a tombstone.
- One binding per `(authenticator, subject)`. Creating it with `put(record,
  None)` makes a second, concurrent bind of the same account on one store a
  `Conflict`.

## 6. Merge classes

`merge_class` is consulted only by `sync` and `pull`, after ADR 0021's ancestor
check: if one side's revision is in the other's `ancestors`, sync fast-forwards
without merging. The class decides only true divergence.

Field-level merging of a `Structured` body needs a real common base: git's own
merge base under `pull`, or the shared parent's retained body under
`sync_with_ancestry` (ADR 0016). Plain `sync(a, b)` calls `sync_with_ancestry`
with no ancestry store, so it merges against an empty body; an empty body isn't
valid JSON, so `structured_merge` returns `NeedsResolution` for every
divergence. In other words, plain `sync` never field-merges a `Structured`
fleet kind — any divergence of `Person`, `IdentityBinding`, `RoleGrant` or
`ChannelConfig` surfaces there as a `SyncConflict`. The table below describes
divergence behaviour **given a common base**; under plain `sync`, read every
"merges" as "conflicts" instead.

| Kind | Class | Divergence behaviour (with a common base; under plain `sync`, any divergence is a `SyncConflict` instead) |
|---|---|---|
| `Person` | Structured | separate field edits merge; the same field edited differently conflicts |
| `IdentityBinding` | Structured | binding one account to two different people conflicts on `person`, so a mistaken bind is surfaced, never kept silently; a `handle` refresh on one side merges with anything else |
| `RoleGrant` | Structured | two different role changes conflict on `role`; a revoke racing an edit is a delete-versus-edit `SyncConflict` (ADR 0021) regardless of base, since that path never reaches the body merge |
| `ChannelConfig` | Structured | `filters` merges per key; `repos` is an array, which merges atomically, so concurrent follows on both sides conflict rather than drop one |
| `LinkToken` | Opaque | only reachable by two independent redemptions (or edits) of one revision, which must be surfaced (§5.1) |
| `AuditEntry` | Opaque | entries are write-once under unique keys, so divergence means a collision or tampering and is surfaced |

**Why not `AppendOnly` for audit entries**, as #277 proposed.
`append_only_merge` (`crates/gonzalo-core/src/merge.rs`) joins two bodies line by
line: `ours`, then the lines of `theirs` past their common prefix. Two different
single-line JSON entries at one key would merge into a two-line body that no
longer decodes, which is the #204 hazard. `Opaque` surfaces the same situation
instead. `TicketEvent` keeps `AppendOnly`; this spec doesn't change it.

**Composite fields merge atomically.** `IdentityBinding`'s `authenticator`,
`email` and `bound_by`, and `RoleGrant`'s `scope` and `granted_by`, are stored
as one-element JSON arrays (`crates/gonzalo-domain/src/fleet/atomic.rs`,
`#[serde(with = "…")]`) rather than as plain objects. `merge_value`
(`crates/gonzalo-core/src/merge.rs`) recurses field-by-field into JSON objects
but compares arrays whole, so without the wrapper a divergent edit to one of
these fields would be merged key by key: an externally-tagged enum
(`FleetActor`, `BindingOrigin`) changed to two different variants merges into a
body with two variant tags that fails to decode, and `IdentityBinding.email`
(`Option<VerifiedEmail>`) changed on both sides could merge one side's
`address` with the other's `verified`, asserting a verification nobody made —
the same family of hazard as #204. Wrapped as a one-element array, a
concurrent change to any of these five fields conflicts instead of producing
an undecodable or unasserted value. `LinkToken` and `AuditEntry` are never
merged (`Opaque`), and `ChannelConfig`'s fields are already plain strings, a
role, an array, and a consumer-defined map, so none of those need wrapping.

## 7. Core change

Principle 5 only: six variants in `RecordKind`
(`Person`, `IdentityBinding`, `RoleGrant`, `ChannelConfig`, `LinkToken`,
`AuditEntry`), their `merge_class` arms, and the assertions in
`merge_class_is_assigned_per_kind`. No new core traits or types.

**Rollout.** `RecordKind` serializes by variant name, so a binary built before
these variants fails to decode the records (a serde error from
`FsStore::read_record`; the daemon rejects a `PutRequest` naming the kind). The
change ships in 0.7.0 together with tombstones, which already require every
gonzalod, CLI and embedded consumer to upgrade together. The CHANGELOG states
that every binary must upgrade before any writer uses the new kinds.

## 8. Relationships

- **ADR 0015.** Its `Principal` is a bearer token with namespace scopes: access
  to *gonzalo*. `Person` is a human with fleet roles: access to *the fleet*.
  They don't reference each other. ADR 0015's "roles" future work concerns
  gonzalod's own API and stays separate; if gonzalod ever authorizes by fleet
  role, that is a new ADR mapping tokens to people.
- **gonzalo#200** (memory scoping by user / agent / session / group) is
  independent. A `Person` id could later serve as a user scope value, but
  neither design needs the other.
- **ADR 0010** is the precedent: a capability layer whose only core touch is
  kind registration.
- **ADR 0021** supplies replicated deletes (grant revocation), ancestor
  fast-forward (token redemption under sync) and `reset` / `collect` (audit
  retention).

## 9. Personal data

Emails and handles are personal data. On the git substrate every write is a
commit, so a value stays in git history after its record is deleted and the
tombstone collected. (`ancestors` hold revisions, not bodies, so they don't add
exposure.) Both email fields and `handle` are optional so a deployment can omit
them; one that needs erasure should not keep `fleet` on a git store.

## 10. Implementation (gonzalo#278)

- `gonzalo-core/src/record.rs`: the six variants and arms.
- `gonzalo-domain`: a `fleet` module with the views, key helpers, `LinkSecret`,
  `RedeemError`, `FleetKeyError`; registered in `lib.rs`; crate description updated.
- `crates/gonzalo/src/lib.rs`: facade re-exports.
- `CHANGELOG.md` `[Unreleased]`: the kinds and the upgrade note.

## 11. Testing

- Every view round-trips through `RecordCodec`; every `KIND` matches.
- `merge_class_is_assigned_per_kind` asserts all six classes.
- Key helpers: escaping is injective (`oidc` issuers containing `:`), person id
  and nonce validation, audit key zero-padding and negative `at` rejection.
- `LinkSecret`: parse/hex round-trip, bad input rejected; a serialized
  `LinkToken` body never contains the secret's hex.
- `redeem`: each `RedeemError`, and success filling `consumed`.
- Against a real `FsStore` (dev-dependency):
  - two redemptions `put` with the same expected revision: exactly one
    `Committed`, one `Conflict`;
  - redeem on store A, sync with store B holding the unredeemed revision: both
    end consumed, no conflict;
  - independent redemptions of one revision on A and B, then sync: one
    `SyncConflict`, neither side changed;
  - audit entry `put(None)` over an existing key: `Conflict`.
