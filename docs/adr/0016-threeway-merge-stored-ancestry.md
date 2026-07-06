# ADR 0016 · 3-way merge with content-addressed stored ancestry

- **Status:** accepted
- **Date:** 2026-07-05
- **Source:** [`docs/superpowers/specs/2026-07-05-threeway-merge-ancestry-design.md`](../superpowers/specs/2026-07-05-threeway-merge-ancestry-design.md)

## Context

`sync` merges divergent records with an **empty base**, so `merge()` is a true
3-way merge only for `AppendOnly` (union is base-agnostic); `Structured` bodies
cannot distinguish a one-sided field edit from a genuine two-sided conflict. The
`merge()` machinery already implements correct 3-way semantics given a base — the
missing piece is the ancestor **body**, which the store does not retain (only the
current record per key is kept; `parent` is a revision hash, not content).

Options weighed for retaining ancestry: (a) a `Store` decorator that records each
body in a content-addressed store, (b) baking retention into every substrate's
`put`, or (c) sync-maintained watermark records. And for resolution scope: the
shared-parent case versus a full lowest-common-ancestor walk over retained
history.

## Decision

We will add **`AncestryStore<S, B>`**, a `Store` decorator over any `S: Store`
plus a `B: BlobStore`. On a committed `put` it writes `body.bytes()` to the
ancestry blob store and delegates. Because `Revision.hash ==
ContentHash::of(body.bytes())`, each version's body is retrievable by its
revision hash. Retention is opt-in and changes no substrate.

`sync` gains `sync_with_ancestry(a, b, ancestry: Option<&dyn BlobStore>)`;
`sync(a, b)` is preserved as the empty-base path. When two divergent records
share a parent revision (`rec_a.parent == rec_b.parent`) whose body is retained,
sync passes that body to `merge()` as the true base; otherwise it falls back to
the empty base (today's behavior). We scope resolution to the **shared-parent
case** — retained blobs carry no parent link, so a full LCA walk is not possible
without also retaining traversable history (out of scope).

## Consequences

- **Positive:** `Structured` divergences from a real base now merge correctly
  (one-sided edits apply; same-field edits conflict). Ancestry reuses the
  existing content-addressed `BlobStore` and `Revision` hash — no substrate
  change, no new addressing scheme. `AppendOnly`/`Opaque`/`Derived` behavior is
  unchanged, and empty-base fallback keeps sync correct when ancestry is absent.
- **Negative:** ancestry retention grows storage (every version's body kept) with
  no GC policy yet; only single-step (shared-parent) divergence gets true 3-way;
  the benefit requires peers to have wrapped their stores before the common write.
- **Revisit if:** multi-step divergence needs true LCA merges (retain parent
  links + walk), or retained ancestry growth needs a GC/retention policy.
