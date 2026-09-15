# ADR 0022 · Fleet access-control records as a capability layer

- **Status:** accepted
- **Date:** 2026-09-14
- **Source:** [`docs/superpowers/specs/2026-09-14-fleet-access-control-records-design.md`](../superpowers/specs/2026-09-14-fleet-access-control-records-design.md)

## Context

Ariel, the fleet's chat bridge (caliban-ai/prospero#67), keeps its identity,
role, channel and audit state in gonzalo and stores nothing itself. It needs six
record types: a person, a binding from an external account to that person, a
role grant, a per-channel configuration, a one-time link token, and an audit
entry. None fits an existing `RecordKind`, and the enum is closed. Principle 5
lets a capability layer register kinds and their merge classes and nothing else
in core, with [ADR 0010](0010-ticket-system-capability-layer.md) as the
precedent, over the layering of [ADR 0008](0008-capability-layers-over-core.md).

Several constraints shaped the design:

- **A name is taken.** [ADR 0015](0015-namespace-scoped-daemon-auth.md) already
  defines `Principal`: a bearer token with namespace scopes, meaning access to
  gonzalo. The Ariel draft used the same word for a human with fleet roles.
- **Auth is namespace-granular.** ADR 0015 grants `read` and `write` per
  namespace, so the key layout decides what a token can be limited to.
- **`AppendOnly` is unsafe for JSON bodies.** The `AppendOnly` merge joins two
  bodies line by line. The first draft proposed it for audit entries, following
  `TicketEvent`. Two different JSON entries at one key would merge into a body
  that no longer decodes, the same hazard gonzalo#204 records for sessions.
- **Deletes now replicate.** The first draft argued link tokens must be marked
  consumed rather than deleted because deletes were local-only and resurrected
  on sync. [ADR 0021](0021-replicated-deletion-with-tombstones.md) changed that,
  so the reasoning had to be redone.
- **Link tokens are secrets.** The git substrate commits every write, so a raw
  token in a record body would persist in history.
- **Emails and usernames are tempting lookup keys.** Handles are renamed and
  re-registered, and some platforms return unverified emails.

Options weighed for the audit class were `AppendOnly` (as drafted) and `Opaque`;
for namespaces, one `fleet` namespace, a namespace per kind, and `fleet` plus
`fleet-audit`; for link-token consumption, delete and mark.

## Decision

We will add a **fleet access-control layer**: six `RecordKind` variants in core
with their merge classes, and typed views in a `fleet` module of
`gonzalo-domain`, re-exported by the facade. The layer is named for fleet access
control generally, so prospero's API (prospero#2) and other surfaces can use the
same people and roles as Ariel.

**Kinds, keys and merge classes.**

| Kind | Key | Merge class |
|---|---|---|
| `Person` | `fleet/people/<person_id>` | Structured |
| `IdentityBinding` | `fleet/identity-bindings/<authenticator>:<subject>` | Structured |
| `RoleGrant` | `fleet/role-grants/<person_id>:<scope>` | Structured |
| `ChannelConfig` | `fleet/channels/<provider>:<channel_id>` | Structured |
| `LinkToken` | `fleet/link-tokens/<token_hash>` | Opaque |
| `AuditEntry` | `fleet-audit/entries/<at_ms>-<nonce>` | Opaque |

The person record is **`Person`**, not `Principal`. It has an opaque id that the
minting consumer generates, never an email or a platform id. Composite ids are
built only by the views' key helpers, which escape each component so an id stays
unambiguous when an OIDC issuer contains `:`.

**Two namespaces.** `fleet` holds the mutable access-control state and
`fleet-audit` holds audit entries. A dashboard or auditor token can then read
the trail without reading link tokens or grants, and a writer that only appends
audit entries needs no write access to grants.

**Merge classes in sync terms.** `merge_class` is consulted only by `sync` and
`pull`, and only after ADR 0021's ancestor check has ruled out a fast-forward;
`put` is pure OCC for every kind.

- The four configuration kinds are `Structured`: separate field edits merge and
  the same field edited two ways is a conflict. Binding one account to two
  different people conflicts on `person`, so a wrong bind is surfaced. Two role
  changes to one grant conflict on `role`. A `ChannelConfig`'s `repos` array
  merges atomically, so concurrent follows conflict rather than drop one.
- `LinkToken` is `Opaque`. A redemption descends from the revision it read, so
  sync fast-forwards a peer that still holds the unredeemed token, which stays
  consumed. Only two independent redemptions of one revision diverge, and those
  must be surfaced.
- `AuditEntry` is `Opaque`, not `AppendOnly`. Each entry is written once under a
  unique key, so divergence means a key collision or tampering and is surfaced
  instead of merged into an undecodable body. `TicketEvent` is unchanged.

**One record per audit entry.** A consumer creates an entry with
`put(record, None)` and never updates it. A `Conflict` means the nonce collided;
the consumer retries with a new nonce. Retention uses the ordinary `reset` and
`collect` on `fleet-audit`.

**Link tokens.**

- **Only a hash is stored.** The record holds a domain-separated blake3 hash of
  the token's 32 random bytes, and the view has no field that can hold the
  secret. An unsalted fast hash is enough because the input is random, not a
  password. gonzalo generates no randomness; the caller supplies the bytes.
- **Redemption marks, never deletes.** `redeem` checks the hash, that the token
  hasn't expired, and that it isn't consumed, then returns the view with
  `consumed` set. The consumer writes it with its read revision as `expected`,
  so concurrent redemptions give one `Committed` and one `Conflict`. Replicated
  deletes would no longer resurrect a deleted token, but marking is still
  chosen: a tombstone keeps no record of who redeemed the token, and because
  redemption checks expiry, a token that reappears after its tombstone is
  collected is already useless. A token may be deleted only after it expires.

**Identifying data.** The platform user id or OIDC `sub` is
`IdentityBinding.subject` and the only lookup key. A username is an optional
`IdentityBinding.handle` snapshot. An IdP-asserted email is an optional
`IdentityBinding.email` with its verified flag. `Person` has a `display_name` and
an optional operator-set contact `email`. **No person is ever resolved by email
or handle.** An account joins a person only through a redeemed link token or an
explicit operator action, which the binding records.

**gonzalo does not enforce fleet roles.** These records are data. The consumer
evaluates them, including Ariel's two-key rule (a command runs at the lower of
the person's role and the channel's ceiling). gonzalod keeps authorizing its own
API with ADR 0015's token `Principal`. ADR 0015's "roles" future work concerns
gonzalod's API and is unaffected; making gonzalod authorize by fleet role would
need a new ADR.

**Relationship to gonzalo#200.** Memory scoping by user, agent, session or group
is independent. A `Person` id could later be a user scope value, but neither
design depends on the other.

**Core change and rollout.** Core gains the six variants, their `merge_class`
arms and test assertions, and nothing else. `RecordKind` serializes by variant
name, so an older binary fails to decode these records: a serde error reading
from a store, and a rejected `PutRequest` on the daemon. The kinds ship in 0.7.0
alongside tombstones, which already require upgrading every gonzalod, CLI and
embedded consumer together; every binary must upgrade before any writer uses the
new kinds.

## Consequences

- **Positive:** Ariel, and later prospero, get versioned, replicated,
  conflict-surfacing identity and audit state with a six-variant core change.
  The two namespaces give the audit trail its own access boundary under existing
  daemon auth. Raw link tokens never reach storage or git history, a token can't
  be redeemed twice on one store, and a redeemed token stays redeemed through
  sync. The views offer no way to resolve a person by handle or email, so a
  consumer that uses them can't be tricked into account takeover by a reused
  handle or an unverified email.
- **Negative:** Core's closed enum now carries six kinds specific to one layer.
  Two offline peers redeeming the same token produce a conflict an operator must
  resolve. Concurrent follows on one channel conflict instead of merging. gonzalo
  can't stop a writer with `fleet` write access from granting itself admin; that
  trust sits with whoever holds the token, as with any namespace. Emails and
  handles are personal data, and on the git substrate they survive in history
  after deletion and collection; the fields are optional, and a deployment that
  needs erasure should not keep `fleet` on git.
- **Revisit if:** a second consumer needs to extend these kinds in ways a closed
  core enum can't serve (the trigger for an open or namespaced kind registry);
  gonzalod needs to authorize its own API by fleet role; offline double
  redemption turns out to be common in practice; or channel `repos` conflicts
  are frequent enough to want a set-merging class.
