# ADR 0001 · Record architecture decisions

- **Status:** accepted
- **Date:** 2026-06-13

## Context

Gonzalo began as a design spec (`docs/superpowers/specs/`) and a set of
per-milestone plans, with the significant decisions captured there. Now that
milestones M1–M6 are implemented, those decisions are spread across long design
documents and commit history — hard to consult and easy to let drift from the
code. The sibling repos caliban and prospero both keep an ADR log; gonzalo had
none.

## Decision

We will keep an Architecture Decision Record log under `docs/adr/`, in
**MADR-lite** format, matching caliban (and prospero once
[caliban-ai/prospero#30](https://github.com/caliban-ai/prospero/issues/30)
lands). Each architecturally significant, hard-to-reverse, or future-constraining
decision gets one append-only record with **Context**, **Decision**, and
**Consequences**. Superseded decisions are marked and linked, never deleted.

This first set of ADRs (0002–0009) is **retrospective**: it documents decisions
already embodied in the M1–M6 code, so the rationale is captured before it is
lost. Decisions from here on are recorded as they are made.

## Consequences

- **Positive:** One durable, greppable home for "why"; new contributors and
  sibling-repo readers meet one consistent format across caliban / gonzalo /
  prospero; design rationale stops drifting from the code.
- **Negative:** An ongoing discipline cost — a significant decision now means
  writing an ADR, not just code. Retrospective ADRs also risk rationalizing
  after the fact rather than capturing the live trade-off.
- **Revisit if:** the MADR-lite format diverges from the agreed cross-sibling
  standard (see [caliban-ai/prospero#30](https://github.com/caliban-ai/prospero/issues/30)),
  or the log proves too heavyweight for the team's cadence.
