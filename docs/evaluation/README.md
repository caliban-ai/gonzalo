# Evaluation

Home for how we measure gonzalo — against competing memory/persistence
layers and (soon) against real backends and standard benchmarks.

Gonzalo sits in the "AI memory layer" space, but from one layer lower than
most: it is a generic, versioned, conflict-aware [`Record`/`Store`
core](../adr/0002-uniform-record-store-core.md) with capability layers
composed on top, not an opinionated memory model. These evaluations exist to
keep that positioning honest — to track, concretely, where gonzalo reaches
parity with the incumbents and where it deliberately does something different.

## Layout

| Directory | Contents |
|-----------|----------|
| [`competitors/`](competitors/) | Per-competitor capability inventories and parity analysis. One subdirectory per competitor, each with a documented-capability inventory + a gonzalo ↔ competitor parity gap matrix. Currently: [`mem0/`](competitors/mem0/) (Mem0 — LLM-driven fact-extraction memory library, the category's most-adopted OSS), [`zep/`](competitors/zep/) (Zep / Graphiti — temporal knowledge-graph engine), and [`letta/`](competitors/letta/) (Letta / MemGPT — stateful-agent runtime with self-editing memory). |

## Conventions

- **Competitors** each get their own directory under `competitors/<name>/`,
  with two files:
  - `capability-inventory.md` — a static, dated snapshot of the competitor's
    *documented* surface, captured from its public docs and source. This is
    the *source* the matrix is derived from; re-baseline it manually (re-fetch
    the upstream docs) before a parity-prioritization pass.
  - `parity-gap-matrix.md` — a gonzalo ↔ competitor gap matrix derived from the
    inventory, marking where gonzalo has reached parity (✅), is partial (🟡),
    or has a gap / out-of-scope call (🔴), with notes citing the relevant ADR.
- Inventories are **snapshots**, not living docs: they record what a
  competitor documented on a given date. Bump the capture date when you
  re-baseline.

## A note on "parity"

Unlike a straight feature-clone target, several competitor capabilities are
🔴 in gonzalo **by design**, not because they are unbuilt — e.g. LLM-driven
fact extraction and bi-temporal reasoning belong to a memory *policy* that
gonzalo intentionally leaves above its storage substrate. Those rows carry a
`by design` note so the matrix reads as a positioning map, not just a backlog.
Each matrix also closes with a short **"Where gonzalo leads"** section for the
inverse: capabilities gonzalo has that the competitor's surface does not
(typed concurrent-write conflicts, multi-writer sync, git-backed history,
backend-as-configuration).

## Coming later

Dated, point-in-time **probes** (live findings against real backends —
filesystem, git, S3, the daemon) and standardized **benchmark** runs (e.g.
LongMemEval-style long-horizon memory suites) will land under this tree once
we start capturing them. Exact structure is deliberately left open until
then; tracked separately.
