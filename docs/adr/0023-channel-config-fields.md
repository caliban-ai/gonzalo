# ADR 0023 · Channel configuration fields, and workspaces instead of repos

- **Status:** accepted
- **Date:** 2026-09-15
- **Amends:** [ADR 0022](0022-fleet-access-control-records.md), whose
  `ChannelConfig` shape and `GrantScope::Repo` variant this replaces. ADR 0022
  stands otherwise.

## Context

[ADR 0022](0022-fleet-access-control-records.md) registered `ChannelConfig` with
the shape the Ariel design spec sketched: a provider, a channel id, a role
ceiling, a list of followed repositories, and a free-form `filters` map. Ariel
has since settled the record's real contract in its own
[ADR 0009](https://github.com/caliban-ai/ariel/blob/main/docs/adr/0009-channel-config.md),
and three of gonzalo's fields don't match it:

- **The key is missing the tenant.** Ariel's ADR 0006 keys every channel by
  provider, tenant and channel. A channel id is only unique within its workspace
  on Slack, and within its team on Teams, so `provider:channel` collides across
  tenants — two different channels would share one record.
- **`repos: Vec<String>` can't express "the whole fleet."** Ariel's `#ops`
  example follows every workspace, including ones created later. A list has to be
  edited whenever a workspace is added, and the omission is silent. Prospero has
  also renamed repositories to workspaces, so the names in this field are
  workspace names, not `owner/name` repos.
- **A free-form `filters` map is unsafe.** Ariel's ADR 0007 fixes which event
  kinds notify and how they are paced. An open map lets a channel opt into kinds
  that pacing forbids, such as per-token output, which floods the channel.

The same repos-versus-workspaces drift affects `GrantScope::Repo`, documented as
`repo:<owner/name>`, while ariel scopes grants per workspace.

The fleet kinds are **unreleased**: the workspace is 0.6.0 and ADR 0022 ships
them in 0.7.0. Changing them now costs nothing; changing them after 0.7.0 would
need a migration, because a stored record's field names are its wire format.

Options weighed: leave gonzalo as it is and have ariel encode its model into
`repos` and `filters` (it would have to amend its own ADR 0009, and the tenant
collision would remain); or amend gonzalo before 0.7.0. We take the second.

## Decision

We will change `ChannelConfig` to carry exactly the fields ariel ADR 0009
specifies, and rename the repo-scoped grant variant.

**Fields.**

```rust
pub struct ChannelConfig {
    pub provider: String,       // "discord", "slack", …
    pub tenant: String,         // guild, workspace or team id
    pub channel: String,        // platform channel id
    pub follows: Follows,       // required, no default
    pub notify: NotifyPreset,   // default All
    pub ceiling: FleetRole,     // default Viewer
}

pub enum Follows { Fleet, Workspaces { names: BTreeSet<String> } }  // non-empty
pub enum NotifyPreset { All, Terminal, Failures }
```

**Key.** `fleet/channels/<provider>:<tenant>:<channel>`, escaped and joined by
the existing key helpers, so the same channel id under two tenants is two
records.

**`follows` is stored tagged**, as ariel ADR 0009 specifies:
`{"kind":"fleet"}` or `{"kind":"workspaces","names":[…]}`. `Workspaces` is a
struct variant because an internally tagged enum cannot hold a bare sequence.
The name set is non-empty, enforced both by the `Follows::workspaces`
constructor and on deserialization, so no decoded value can break the invariant.

**`follows` merges atomically.** It carries ADR 0022's one-element-array
wrapper, so the stored field is `"follows":[{"kind":"fleet"}]`. Two concurrent
follow changes conflict rather than splicing two sets together or mixing a
`kind` from one side with `names` from the other. This keeps the behaviour the
old `repos` array had, where an array compared whole.

**`notify` and `ceiling` default** to `All` and `Viewer` when absent, so a
record written without them is still valid and grants nothing extra. `follows`
has no default: a channel never silently follows the whole fleet.

**gonzalo still doesn't enforce any of this.** The presets, the ceiling and the
followed set are data. Ariel decides what a channel hears and what a command may
do, as ADR 0022 says.

**`GrantScope::Repo(String)` becomes `GrantScope::Workspace(String)`**, keyed
`workspace:<name>`, matching prospero's rename and ariel's per-workspace grants.
`FleetRole` also gains a `Default` of `Viewer`, the least privileged.

## Consequences

- **Positive:** A channel record now says what ariel needs it to say, and both of
  ariel ADR 0009's worked examples are one record. Channels can't collide across
  tenants. A channel cannot opt into event kinds that pacing forbids, because the
  presets are a closed set. "Follow the whole fleet" survives a new workspace
  without an edit. One rename settles the repos-versus-workspaces drift across
  grants, link tokens and channels while nothing has shipped.
- **Negative:** This is a breaking change to kinds ADR 0022 accepted, so any
  consumer pinning gonzalo from git — ariel does, at `f537da7` — must re-pin and
  update its records. A channel cannot ask for a mix of event kinds outside the
  three presets, and a per-channel need beyond them requires a new preset here
  and in ariel. `follows` stored inside the atomic wrapper is one level deeper
  than ariel ADR 0009's bare tagged object, so a reader of the raw JSON sees
  `[{"kind":…}]`.
- **Revisit if:** a platform appears whose channels need more than provider,
  tenant and channel to address; presets prove too coarse for real channels (the
  trigger for per-workspace presets within one channel); or a consumer other than
  ariel needs a different channel model, which would argue for moving this kind
  out of the shared fleet layer.
