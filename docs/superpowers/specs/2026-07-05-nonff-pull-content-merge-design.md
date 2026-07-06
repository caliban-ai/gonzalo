# Non-fast-forward git pull via content-aware merge

- **Ticket:** gonzalo#7
- **Date:** 2026-07-05
- **Status:** Accepted
- **Records:** ADR 0017
- **Refs:** `crates/gonzalo-store-git/src/lib.rs`, `crates/gonzalo-core/src/merge.rs`,
  ADR 0016 (3-way merge with stored ancestry)

## Problem

`GitStore::pull` is fast-forward-only: on divergence it errors with
`"non-fast-forward pull requires manual merge"`. Replication cannot reconcile a
local branch that has its own commits against a remote that has moved on.

Unlike sync (#2/ADR 0016), git already retains history — the **merge base commit**
is the true common ancestor — so 3-way merge needs no separate ancestry store.
The reconciliation must use gonzalo's class-aware `merge()` on record bodies, not
git's line-based content merge, which would mangle structured JSON records.

## Design

### `pull` return type

```rust
pub struct PullConflict {
    pub key: RecordKey,
    pub local: Box<Record>,
    pub remote: Box<Record>,
}
pub struct PullReport {
    pub fast_forwarded: bool,          // a clean fast-forward (or up-to-date)
    pub merged: Vec<RecordKey>,        // records 3-way-merged into a merge commit
    pub conflicts: Vec<PullConflict>,  // NeedsResolution — kept local, for the caller
}
pub async fn pull(&self, remote: &str, branch: &str) -> Result<PullReport>;
```

There are no existing callers, so widening `Result<()>` → `Result<PullReport>` is
free. Up-to-date → empty report. Fast-forward → today's advance + checkout, with
`fast_forwarded: true`.

### Non-fast-forward merge

`base = repo.merge_base(local, remote)` (an empty tree if histories are
unrelated). Diff `base→local` and `base→remote` to classify each record path:

- changed **only on remote** → apply remote's blob (or its deletion);
- changed **only on local** → keep local (already in the tree);
- changed on **both** → load the base/local/remote `Record` for the path and run
  `merge(kind.merge_class(), base_body, local_body, remote_body)`:
  - `Merged(body)` → rebuild the record with a fresh revision (counter =
    `max(local, remote) + 1`, hash of the merged body — mirroring `sync`'s
    `build_merged`), write it → `merged`;
  - `NeedsResolution` → keep the local version → `conflicts`.

Reconciled entries are staged in the repo index; `write_tree` yields the merged
tree. Create a **merge commit with both parents** (`[local, remote]`), point the
branch ref at it, and `checkout_head --force`. Return the report.

### Edge cases

- **add/add** of the same key with differing bodies → `merge()` with an empty
  base: append-only unions; structured → conflict, kept local.
- **modify/delete** → conflict, kept local.
- **one-sided delete** (remote deleted, local unchanged) → apply the deletion.
- **unrelated histories** (no merge base) → empty base tree; every overlapping
  path is add/add.

### Why gonzalo `merge()`, not git's

git's `merge_trees` does line-based content merging: for a structured JSON record
it can silently produce a wrong merge or a spurious conflict. Routing both-sided
changes through `merge(class, …)` applies the record's declared semantics
(`AppendOnly` union, `Structured` field-3-way, `Opaque`/`Derived` rules).

## Testing

Real git repos via `git2` + tempdirs — a shared base commit, then divergent
commits on local and a remote:

- disjoint record edits on each side → a merge commit with **two parents**, both
  edits present, `merged` populated;
- a genuinely conflicting record (e.g. a `Checkpoint`/`Opaque` changed on both
  sides) → reported in `conflicts` with local retained;
- a one-sided remote change → applied into the local tree;
- fast-forward divergence still fast-forwards (`fast_forwarded: true`);
- up-to-date pull is a no-op empty report.

## Scope boundaries (YAGNI)

- Merge (not rebase). No rename detection — records are content-addressed by a
  stable path, so renames don't arise.
- Conflicts keep local and are reported; no conflict-marker files (records are
  structured JSON, not line-mergeable text).
- No change to `push` or to the `Store` surface.

## Acceptance criteria

- [ ] Non-FF `pull` performs a content-aware 3-way merge via gonzalo `merge()`
      and creates a two-parent merge commit.
- [ ] Auto-merged records land in the merge commit; unresolved records are
      reported and keep local.
- [ ] Fast-forward and up-to-date paths preserved; `pull` returns `PullReport`.
- [ ] Tests over real diverged git repos cover merge, conflict, one-sided, FF,
      and up-to-date.
- [ ] ADR 0017 records the semantics.
