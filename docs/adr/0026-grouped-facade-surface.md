# ADR 0026 · A grouped facade surface

- **Status:** accepted
- **Date:** 2026-09-20
- **Amends:** [ADR 0009](0009-workspace-layout-and-facade.md), whose single-facade
  decision stands. What changes is the *shape* of that facade's public surface,
  not whether there is one.

## Context

The `gonzalo` facade re-exports **74 names at its top level** with default
features on, and more as features are enabled: 30 from `gonzalo-core` and 44
from `gonzalo-domain`. Many are ordinary English nouns — `Actor`, `State`,
`Provider`, `Container`, `Link`, `Topic`, `Turn`, `Session`, `Priority`,
`Resolution`, `Person`. Two consequences follow.

**They collide with a consumer's own types.** A program that has its own
`State` or `Actor` cannot write `use gonzalo::*`, and even a targeted import
reads as a claim on a name the consumer had first.

**They hide which layer a name belongs to.** `State` is a ticket state;
`Actor` is a ticket actor; neither says so at the import site, and a reader has
to go looking to find out. The flat surface also cannot express two layers using
the same word: `gonzalo-ticket` and `gonzalo-graph` both define a `Page`, and a
flat facade can only ever re-export one of them.

The alternative is already in the tree twice over. The fleet records are
re-exported both flat *and* as `gonzalo::fleet`, and one layer down
`gonzalo-domain` is **already** organised into modules — `checkpoint`, `codec`,
`fleet`, `memory`, `session`, `ticket`. The flattening is something the facade
does on the way out, not a structure the domain lacks.

## Decision

**The facade mirrors the module structure the crates already have.** It stops
flattening `gonzalo-domain`, and each capability layer becomes a module named
after its crate:

| Module | Contents |
|---|---|
| root | the record and store core — `Record`, `RecordKey`, `RecordKind`, `Revision`, `Body`, `Meta`, `Identity`, `ContentHash`, `KeyPrefix`, `Store`, `BlobStore`, `AncestryStore`, `PutResult`, `DeleteResult`, `Conflict`, `CoreError`, `Result`, `MergeClass`, `MergeOutcome`; the operations `sync`, `sync_with_ancestry`, `merge`, `collect`, `reset`, `reset_as`, `gc_blobs`, `now_ms` and their reports; and the substrates `FsStore`, `GitStore`, `S3Store`, `ServerStore` |
| `memory` | `MemoryTier`, `Topic` |
| `session` | `Session`, `Turn` |
| `checkpoint` | `Checkpoint` |
| `codec` | `RecordCodec` |
| `ticket` | the domain types (`Ticket`, `TicketBody`, `TicketEvent`, `State`, `StateCategory`, `Actor`, `ActorRole`, `Priority`, `PriorityLevel`, `Resolution`, `Provider`, `Container`, `Link`, `LinkKind`, `LinkTarget`, `BodyFormat`) **and** the source layer (`TicketSource`, `Capabilities`, `Cursor`, `FieldMapping`, `InMemorySource`, `Page`, `SourceError`, `StateMapping`, `StateSignal`, `record_key`, `scoped_uid`) with its connectors (`GitHubSource`, `JiraSource`, `LinearSource`, `GitLabSource`, `AsanaSource`) |
| `fleet` | unchanged — it already exists |
| `graph`, `vector`, `knowledge` | the feature-gated capability layers, each under its own name |

**The root holds the record and store core, and nothing else.** That is the part
every consumer touches whatever else they use, it is what the facade is *for*,
and none of it has ever been ambiguous: nobody is confused about what `Record`
or `Store` means in a crate called `gonzalo`. Everything that is a *domain
noun* — a thing the store happens to hold — moves under the module that names
its domain.

**The grouping is mirrored, not invented.** Where a module already exists one
layer down, the facade uses it verbatim, including `checkpoint` and `codec`,
which hold one item each. A module per crate-module is a rule a reader can
predict; "the ones we judged to be confusing" is not.

**The flat names are removed in the same release that adds the modules
(0.8.0).** There is no deprecation window. The project is pre-1.0, the surface
is versioned, and carrying both spellings would leave the collision the whole
change exists to fix — `gonzalo::Actor` would still occupy the consumer's
namespace while `gonzalo::ticket::Actor` sat beside it.

Because the compiler will say only "unresolved import" and not where the name
went, **the release ships the mapping the compiler cannot**: the changelog entry
and the guide carry an old-path → new-path table covering every moved name, and
the release notes lead with it. That table is part of the deliverable, not a
nicety, and [#319](https://github.com/caliban-ai/gonzalo/issues/319) carries it
as an acceptance criterion.

## Consequences

- **Positive:** a consumer's own `State`, `Actor` or `Page` stops colliding with
  gonzalo's, and `use gonzalo::*` becomes a reasonable thing to write again.
  Every import says which layer it came from. Two layers can use the same word —
  `ticket::Page` and `graph::Page` can both exist, which the flat surface made
  impossible. The facade stops being a hand-maintained list that drifts from the
  crates it fronts: it mirrors them, so the next domain module appears for free.
- **Negative:** every existing consumer's imports break at 0.8.0, in one step,
  with errors that name the missing symbol but not its new home. That is the
  cost of skipping the deprecation window, and the old→new table is a mitigation
  rather than a substitute — a consumer still edits every import by hand. The
  root/module boundary also has to be defended over time: the pull to promote
  "just one more" convenient name back to the root is what produced the flat
  surface in the first place.
- **Neutral:** no crate below the facade changes. This is a re-export layout, so
  `gonzalo-domain` and the capability crates keep the structure they already
  have, and a consumer depending on those crates directly sees nothing new.
