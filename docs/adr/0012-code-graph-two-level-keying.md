# ADR 0012 · Two-level keying for the code graph

- **Status:** accepted
- **Date:** 2026-07-03
- **Source:** [`docs/superpowers/specs/2026-07-03-code-graph-capability-design.md`](../superpowers/specs/2026-07-03-code-graph-capability-design.md)

## Context

`gonzalo-graph` parses source with tree-sitter into a `CodeGraph` of `Symbol`s
and name-based `Reference`s (ADR 0008 places it as a capability layer over core).
To serve agents working in isolated git worktrees — including the product case
where Caliban runs a fleet of sub-agents, each in its own worktree, on the same
repo — the graph must be persisted, kept fresh, and queried per agent.

The naive design keys a file's graph by `(repo, path)` with last-writer-wins.
That is wrong the moment two worktrees edit the same path differently: both
resolve to one key, the second write clobbers the first, and an agent then
queries a graph describing *another worktree's* code — actively lying about the
tree it is editing. "They merge later" does not help; during the divergence
window each agent needs a graph matching *its own* tree now.

The error is conflating two distinct identities:

- **Storage identity** — what dedups. A file's graph slice is a pure function of
  its content and the grammar version.
- **Query identity** — what an agent resolves against. This is per-worktree (per
  *view*), because divergent worktrees are the whole point.

This is exactly git's blob/tree split, which fits because these are literally git
worktrees. A related question — which engine backs the queryable graph (in-memory,
SQLite, Cozo, …) — must not be allowed to leak into the durable source of truth.

## Considered options

- **Key everything `(repo, path)`, last-writer-wins.** Rejected: clobbers
  divergent worktrees (above).
- **Key everything `(repo, worktree, path)`, no indirection.** Correct, but every
  worktree stores a full copy of every slice, including unchanged files — N×
  storage across a fleet.
- **Two-level: content-addressed slices + per-view manifests.** Correctness *and*
  dedup. Chosen.

## Decision

We will key the code graph at two levels, and never by `(repo, path)`.

- **Storage layer — content-addressed, deduped slices.** A file's graph slice
  (the symbols it defines, the references it emits) is stored keyed by
  `(file_content_hash, grammar_version)`, **write-if-absent**. Byte-identical
  files across worktrees share one slice. This is what `Body::Blob`
  (content-addressed record bodies) is for.
- **Identity layer — per-view manifests.** Each workspace *view* (a worktree, or
  in the product case an arbitrary target-repo checkout) owns a manifest
  `(repo, view_id) → { path → content_hash }`. An agent resolves a path through
  *its* manifest to *its* slice. The resolvable key is per-view; storage stays
  content-addressed. Neither layer is keyed `(repo, path)`.
- **Slices are path-agnostic.** `Symbol` and `Reference` carry line ranges only,
  not a `file` field; the path is supplied by the manifest at assembly time. A
  rename (byte-identical content) is a manifest repoint, not a reparse — path is
  identity, not content.
- **Resolution happens at assembly time, never in a stored slice.** Slices are
  stored raw, name-based, and unresolved; name resolution runs over the slice set
  a manifest assembles, per query. Resolution **must tolerate missing targets** —
  a file absent from a view yields honest dangling references, which is the truth
  of that tree. (`gonzalo-graph`'s existing "references are unresolved" state is
  therefore the correct layering, not merely unfinished.)
- **Sync is set-reconciling, not append-only.** A view's manifest must equal the
  set of files present in that view. The driver is `git diff` / `git status`,
  which reports deletes (`D`) as first-class alongside adds/modifies (`A`/`M`) and
  reconciles the manifest to the tree by construction. A write-only file-watcher
  is insufficient: it leaves ghost paths resolving to dead content. Any watcher
  path must handle unlink events plus a periodic full reconcile.
- **Storage-engine invariant.** No query engine (in-memory, SQLite, Cozo, …) ever
  sits under the `Store` substrate (the durable, versioned, conflict-aware source
  of truth). Engines back only the *derived, regenerable* index layers
  (`GraphStore`, and optionally `VectorIndex`), which are swappable behind their
  traits. Content-addressed slices are a `Body::Blob` / KV concern, distinct from
  the assembled queryable graph the engine backs.

## Consequences

- **Positive:** Divergent worktrees each query a graph matching their own tree —
  correctness under a fleet — while byte-identical files share one slice (dedup,
  no N× blow-up). Renames are free. Deletion is a per-view manifest edit with no
  "whose delete wins" ambiguity, and slices reclaim through one GC path (an edit
  orphans an old-content hash exactly as a delete does). Conflicts nearly vanish:
  slices are write-if-absent (same hash ⇒ same bytes), manifests are
  single-writer-per-view. The engine choice stays reversible behind a trait.
- **Negative:** Two-level indirection is more moving parts than one keyed table:
  a content-addressed blob store (`Body::Blob`, pulled forward from its reserved
  milestone), per-view manifest records with a `MergeClass::Derived` arm, GC
  (refcount or mark-sweep over live manifests), and assembly-time resolution that
  must handle dangling references. Resolution cost moves from write time to query
  time.
- **Revisit if:** views stop being git worktrees (the blob/tree analogy weakens);
  or single-writer-per-view no longer holds (concurrent writers to one manifest
  would need real merge, not last-writer-wins on the rare race); or profiling
  shows assembly-time resolution is too costly to run per query and a resolved
  cache must be introduced (without baking resolution back into stored slices).
