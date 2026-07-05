# Per-kind chunking for the knowledge store

- **Ticket:** gonzalo#29
- **Date:** 2026-07-05
- **Status:** Accepted
- **Refs:** ADR 0011 (vector⋈graph, chunking named as a future refinement), gonzalo#16 (knowledge store phase 1)

## Problem

The knowledge store embeds **one document per record** (phase 1). For long
records this is lossy retrieval: a whole session transcript or a long memory
tier collapses to a single averaged vector, so a query matching one turn or
section competes against the entire document's averaged embedding and ranks
poorly.

## Goal

Embed records at **chunk granularity** so a query matching one turn/section hits
that piece's own vector directly — while keeping `query` results one-per-record
by default, and preserving the existing public surface (no API break).

## Scope

`crates/gonzalo-knowledge/src/lib.rs` **only**. No cross-crate changes, no
`VectorIndex` trait change, no mutation of source records.

## Design

### 1. Chunk extraction — `chunk(record) -> Option<Vec<String>>`

The knowledge gate is unchanged: `Checkpoint` and `GraphManifest` return `None`
(not knowledge-bearing). Knowledge-bearing kinds return ordered chunk texts:

| Kind          | Chunks                                                        |
|---------------|--------------------------------------------------------------|
| `Session`     | one per turn (`turn.text`); `name` prepended to chunk 0       |
| `Topic`       | one per bullet; `slug` prepended to chunk 0                   |
| `MemoryTier`  | `content` split on blank lines (paragraphs); `name` prepended to chunk 0 |
| `Ticket`      | chunk 0 = `title` + labels; then `body.markdown` paragraphs   |
| `TicketEvent` | single chunk (`body`) — the one-document fallback shape       |

If a kind's structured split yields a single piece, that is the **one-document
fallback** and behaves exactly as phase 1 does today.

`knowledge_text` stays `pub` and is **redefined** as:

```rust
pub fn knowledge_text(record: &Record) -> Option<String> {
    chunk(record).map(|cs| cs.join("\n"))
}
```

This keeps it consistent with `chunk` and preserves its existing tests.

Paragraph splitting: split text on blank-line boundaries, trim each piece, drop
empties. A helper `split_paragraphs(&str) -> Vec<String>`.

### 2. Chunk keying — ordinal in the `id` field

The `VectorIndex` is keyed by `RecordKey = namespace/collection/id`, and
`query`'s `KeyPrefix` filter matches on `namespace` + `collection` **only**
(never `id`). So the chunk ordinal goes in `id`, leaving the filter intact.

```rust
fn chunk_key(parent: &RecordKey, ordinal: usize) -> RecordKey;   // id = "{parent.id}\u{1f}{ordinal}"
fn parent_key(chunk: &RecordKey) -> (RecordKey, usize);          // rsplit_once('\u{1f}')
```

Separator is **ASCII Unit Separator (`\u{1f}`)**, not `#` or `/`: real ids
already contain those (e.g. ticket id `o/r#1`), so a control char guarantees
`rsplit_once` recovers the parent unambiguously. Every index entry carries an
ordinal (single-chunk fallback gets `\u{1f}0`), so parsing is never ambiguous.
The `Store` still holds exactly one record at the clean parent key; only the
`VectorIndex` gains per-chunk entries. Chunk keys never become store paths.

### 3. Ingest with delta cleanup

`KnowledgeStore` gains `chunk_counts: Mutex<HashMap<RecordKey, usize>>`.

`ingest(key)`:
1. `store.get(key)` → absent → `false`. `chunk(record)` → `None` → `false`.
   Else `Some(chunks)`, `new = chunks.len()`.
2. Read `old = counts.get(key).unwrap_or(0)` under a **scoped** lock (released
   before any `.await` — never hold a `std::sync::Mutex` guard across an await).
3. `index.remove(chunk_key(key, i))` for `i in new..old` — drops orphaned chunk
   vectors when a re-ingested record shrank.
4. Embed each chunk and `index.upsert(chunk_key(key, i), vector)`.
5. Write `counts.insert(key, new)` under a scoped lock.

**Lifecycle rationale:** the only index backend today is the in-memory
`MemoryVectorIndex`, so the count map and the index share a lifecycle — a
process restart wipes both and everything is re-ingested fresh, so no orphan can
survive a restart. This keeps cleanup self-contained in `gonzalo-knowledge` with
no source-record mutation.

**Follow-up (out of scope):** when a *persistent* index backend lands
(gonzalo#9), it must pair with a persistent count map, or this should be
promoted to a `remove_prefix`/id-prefix delete on the `VectorIndex` trait. File
a follow-up ticket.

### 4. Query

**`query(text, k, filter) -> Vec<Hit>`** — de-duped, one hit per record.
Signature unchanged. Over-fetches `k * OVERFETCH` chunk matches (`const
OVERFETCH: usize = 8`), groups by parent key keeping the best chunk score, sorts
by score descending, takes the top `k` parents, and resolves each to a record.
Documented limitation: a record with many matching chunks can crowd the
over-fetch window, so pathological cases may return `< k` hits. `k == 0` fetches
nothing. `query_in_subgraph` is unchanged — it calls `self.query` and the
chunking is transparent to it.

**`query_chunks(text, k, filter) -> Vec<ChunkHit>`** — new, chunk-granular view.
Raw top-`k` chunk matches (no de-dup); each resolved to its parent record and
chunk:

```rust
pub struct ChunkHit {
    pub record: Record,   // the parent record
    pub score: f32,
    pub ordinal: usize,   // which chunk matched
    pub text: String,     // that chunk's text
}
```

Resolution: `parent_key(match.key)` → `store.get(parent)` → `chunk(record)` →
`text = chunks[ordinal]`.

## Testing (TDD)

- `chunk` per-kind shapes: multi-turn `Session` → N chunks; `Topic` bullets → N
  chunks; a single-piece kind → one-chunk fallback.
- **Acceptance:** a multi-turn session where a query matches exactly one turn
  ranks that record first (the case one-document embedding got wrong).
- De-dup: two turns of one session both match → `query` returns a single hit for
  that record, with the best chunk's score.
- `query_chunks` returns individual chunk hits with correct `ordinal` and
  `text`.
- **Re-ingest shrink removes orphans:** ingest a record with 3 chunks,
  re-ingest with 1, assert content unique to a removed chunk is no longer
  retrievable.
- Back-compat: existing `knowledge_text_per_kind` and
  `ticket_text_includes_title_body_labels` tests stay green unchanged.

## Acceptance criteria (from the ticket)

- [x] Per-kind `chunk` with sensible defaults; one-document remains the fallback.
- [x] Chunk keys resolve to parent records; hits de-dup to parent by best score.
- [x] Tests: a multi-turn session where a query matches one turn ranks correctly.
