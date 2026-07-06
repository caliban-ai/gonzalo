# 3-way merge with stored ancestry

- **Ticket:** gonzalo#2
- **Date:** 2026-07-05
- **Status:** Accepted
- **Records:** ADR 0016
- **Refs:** `crates/gonzalo-core/src/{sync,merge,record,store}.rs`,
  `docs/superpowers/plans/2026-06-05-gonzalo-m2-substrates-sync.md` (empty-base note)

## Problem

`sync` reconciles two stores. On a divergence it calls `merge(class, base, ours,
theirs)` with an **empty base** (`Body::Inline(vec![])`). That is correct for
`AppendOnly` (a union is base-agnostic) but wrong for `Structured` bodies: with
no real common ancestor, a field changed on only one side cannot be
distinguished from a genuine two-sided conflict, so the merge is not truly
3-way.

The blocker is retrieval: the store keeps only the current record per key, and
`parent` is a `Revision` (counter + hash), not the ancestor's content. An
overwritten `Inline` body is gone. True 3-way needs the ancestor **body**.

## Design

### Ancestry retention — `AncestryStore<S, B>` (gonzalo-core)

A `Store` decorator wrapping any `S: Store` plus a `B: BlobStore`. On a
**committed** `put`, it writes the record's `body.bytes()` to the ancestry blob
store, then delegates; `get`/`list` delegate unchanged.

```rust
pub struct AncestryStore<S, B> { inner: S, ancestry: B }
impl<S: Store, B: BlobStore> AncestryStore<S, B> {
    pub fn new(inner: S, ancestry: B) -> Self;
    pub fn ancestry(&self) -> &B;
}
```

Because `put_blob(content)` keys by `ContentHash::of(content)` and
`Revision.hash == ContentHash::of(body.bytes())`, each version's body is
retained under its own revision hash and is later retrievable by
`get_blob(revision.hash)`. `put_blob` is write-if-absent/idempotent, so
re-retaining a body is free. This is **opt-in and touches no substrate**
(fs/git/s3/server unchanged).

Only `Inline` bodies benefit: for a `Blob` body, `body.bytes()` is the reference
hash (not the content), but `Blob` bodies are `Derived` (code-graph slices),
whose merge ignores the base — so this is immaterial.

### Sync uses the shared parent as the real base

`sync(a, b)` is preserved (empty base — the sole caller, the CLI, is untouched)
and delegates to a new `sync_with_ancestry(a, b, ancestry: Option<&dyn
BlobStore>)`. In the both-present divergence branch, the base is resolved:

```text
base = if ancestry = Some(anc)
          and rec_a.parent == rec_b.parent == Some(rev)
          and anc.get_blob(rev.hash) == Some(bytes)
       then Body::Inline(bytes)          // the true common ancestor
       else Body::Inline(Vec::new())     // safe fallback — today's behavior
```

So when both peers edited once from a shared, retained base, `merge()` gets true
3-way semantics; deeper or asymmetric divergence (different parents, or an
un-retained base) degrades to the current empty-base union. `merge()` is
unchanged.

**Why shared-parent only.** Retained blobs are content only; they carry no
parent link, so histories are not traversable to a lowest common ancestor. The
`rec_a.parent == rec_b.parent` case covers the common "both edited once from a
shared base" scenario; a full LCA walk (retaining parent links + traversal) is
out of scope.

### Behavior across merge classes (the ticket's verification item)

- **Structured** — the payoff. Disjoint field edits from the real base
  auto-merge; the same field changed differently now correctly conflicts
  (empty base would mis-handle both).
- **AppendOnly** — same merged result (union is base-agnostic), now anchored on
  the real base.
- **Opaque** — still `NeedsResolution`. **Derived** — still takes `ours`.

## Testing

- **`AncestryStore`:** a committed `put` retains `body.bytes()` under the
  revision hash and serves it via the ancestry `BlobStore`; `get`/`put`/`list`
  delegate (run the shared `Store` conformance suite against the decorator).
- **Sync 3-way:** a `Structured` divergence from a shared, retained base
  **auto-merges** disjoint field edits **with** ancestry but **conflicts**
  **without** it — proving the base changes the outcome. Plus: shared-parent
  fetch resolves the base; fallback when ancestry is absent, when there is no
  shared parent, or when the base blob is missing; `AppendOnly` still
  auto-merges either way.

## Scope boundaries (YAGNI)

- Shared-parent (single-step divergence) only; no LCA walk / traversable history.
- Ancestry retention is opt-in via the decorator; no substrate `put` changes and
  no ancestry GC/retention policy this pass (a growth note for follow-up).
- No change to `merge()` or `MergeClass`.

## Acceptance criteria

- [ ] `AncestryStore` retains each committed body by revision hash and delegates
      the `Store` surface.
- [ ] `sync_with_ancestry` passes the shared parent's body as the base when
      retrievable; `sync(a, b)` keeps the empty-base fallback.
- [ ] Structured divergence auto-merges with ancestry, conflicts without;
      AppendOnly/Opaque/Derived behavior verified.
- [ ] ADR 0016 records the mechanism and shared-parent scope.
