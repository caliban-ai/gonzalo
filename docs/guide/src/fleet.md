# Fleet access-control records

Gonzalo stores the identity, role and audit state for **Ariel**, the caliban-ai
fleet's chat bridge, and for other consumers that share the same model
(prospero's API among them). Six typed views in `gonzalo-domain`'s `fleet`
module, re-exported by the `gonzalo` facade as `fleet::*`, cover a person, the
external accounts bound to that person, role grants, per-channel
configuration, one-time link tokens, and the audit trail. The design is
recorded in [ADR 0022](./adr/0022-fleet-access-control-records.md), whose
`ChannelConfig` shape and repo-scoped grant variant were amended by
[ADR 0023](./adr/0023-channel-config-fields.md).

## The six kinds

| Kind | For |
|---|---|
| `Person` | A human with fleet roles, keyed by an opaque id the minting consumer generates — never an email or a platform id. |
| `IdentityBinding` | Ties one external account (a Discord user, an OIDC subject, …) to a `Person`. |
| `RoleGrant` | One person's role for one scope (the whole fleet, or one workspace). Revoking a role is a delete, not an edit. |
| `ChannelConfig` | A chat channel's configuration: what it follows, how it's notified, and the highest role a command in it may run with. |
| `LinkToken` | A one-time, expiring token that lets a person claim an external account. |
| `AuditEntry` | One write-once record of who did what, to what, when, and with what result. |

## Two namespaces

Records live in two namespaces:

- **`fleet`** holds the mutable access-control state: people, identity
  bindings, role grants, channel configs and link tokens.
- **`fleet-audit`** holds the audit trail.

They're kept apart because [ADR 0015](./adr/0015-namespace-scoped-daemon-auth.md)
scopes daemon auth per namespace: a dashboard or auditor token can be granted
`read` on `fleet-audit` without also being able to read link tokens or role
grants, and a writer that only appends audit entries needs no write access to
the access-control state itself.

## Key shapes

Composite ids are built only by each view's key helpers, which escape every
component so an id stays unambiguous even when a component (an OIDC issuer,
say) itself contains `:`.

| Kind | Key |
|---|---|
| `Person` | `fleet/people/<person_id>` |
| `IdentityBinding` | `fleet/identity-bindings/<authenticator>:<subject>` |
| `RoleGrant` | `fleet/role-grants/<person_id>:<scope>` |
| `ChannelConfig` | `fleet/channels/<provider>:<tenant>:<channel>` |
| `LinkToken` | `fleet/link-tokens/<token_hash>` |
| `AuditEntry` | `fleet-audit/entries/<at>-<nonce>` |

A `ChannelConfig`'s key includes the tenant (the guild, workspace or team id)
because a channel id is only unique within its tenant on some platforms —
without it, the same channel id under two different tenants would collide on
one record.

## Gonzalo stores; it doesn't decide

**Gonzalo stores these records but never evaluates the roles in them.** The
consumer reads them and decides — for Ariel, that means the two-key rule,
`effective = min(person_role, channel_ceiling)`. `gonzalod` keeps authorizing
its own API by ADR 0015's token `Principal`, which is unrelated to `FleetRole`;
making `gonzalod` itself authorize by fleet role would need a new ADR.

## Link tokens

- **Only a hash is stored.** `LinkToken.token_hash` is a domain-separated
  blake3 hash of the token's 32 random bytes. The view has no field that can
  hold the secret itself, so a raw token never reaches storage or, on the git
  substrate, commit history.
- **Redemption marks the token consumed; it never deletes it.** `redeem`
  checks the secret, that the token hasn't expired, and that it isn't already
  consumed, then returns a copy with `consumed` set, written with the read
  revision as `expected` so two concurrent redemptions produce one `Committed`
  and one `Conflict`. Marking instead of deleting keeps no record of who
  redeemed the token, unlike a tombstone.
- **A token may be deleted only after it expires.** Because redemption checks
  expiry, a token that reappeared after deletion (say, resurrected by a sync
  with a peer that never saw the delete) would already be useless — so
  deleting before expiry is not offered.

## Audit entries are write-once

An `AuditEntry` is created with `put(record, None)` and never updated. A
`Conflict` means the key's `nonce` collided; the consumer retries with a new
nonce. Retention uses the ordinary [`reset` and `collect`](./deletion.md) on
`fleet-audit`, the same as any other namespace.

## No person is ever resolved by email or handle

The platform user id or OIDC `sub` — `IdentityBinding.subject` — is the only
lookup key. A username (`handle`) and an IdP-asserted email are both optional
snapshots, kept for display only. `Person.email` is likewise an optional,
operator-set contact address, never used to match an account to a person. An
account joins a person only through a redeemed link token or an explicit
operator action, which the identity binding records — handles are renamed and
re-registered, and some platforms return unverified emails, so neither is safe
as a lookup key.

## Merge behaviour

- **The four configuration kinds** — `Person`, `IdentityBinding`, `RoleGrant`
  and `ChannelConfig` — are `Structured`. With a real common base (git's merge
  base under `pull`, or a retained shared parent under `sync_with_ancestry`),
  separate field edits merge and the same field edited two ways conflicts.
- **`LinkToken` and `AuditEntry` are `Opaque`.** A `LinkToken` redemption
  descends from the revision it read, so sync fast-forwards a peer that still
  holds the unredeemed token; only two independent redemptions of one revision
  diverge, and that's surfaced. An `AuditEntry` is written once under a unique
  key, so any divergence means a key collision or tampering, and is surfaced
  rather than merged into a body that wouldn't decode.
- **Composite fields are stored as one-element arrays so they merge as a
  single unit.** An externally-tagged actor/origin enum (`bound_by`,
  `granted_by`), a verified-email pair, a `RoleGrant`'s `scope`, and a
  `ChannelConfig`'s `follows` are all wrapped this way, so a concurrent change
  can never mix one side's enum variant (or one field of a pair) with the
  other's into a body that fails to decode.
- **Plain `sync` has no common base.** Unlike `pull` and `sync_with_ancestry`,
  plain `sync` doesn't retain a shared ancestor, so under it every concurrent
  edit to one of the four `Structured` kinds — not just the atomically-wrapped
  fields — surfaces as a conflict rather than merging field by field. Choose
  `pull` or `sync_with_ancestry` when field-level merging of fleet records
  matters.

See [Storage backends](./storage.md) and
[Deletion, reset & collection](./deletion.md) for how merge classes, sync and
tombstones work in general.
