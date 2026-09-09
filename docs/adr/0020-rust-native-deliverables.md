# ADR 0020 · Rust-native deliverables; the daemon is the non-Rust boundary

- **Status:** accepted
- **Date:** 2026-09-09

## Context

gonzalo ships Rust crates. Nothing in this repository ever said so.

ADR 0009 records the Cargo workspace and the `unsafe_code = "forbid"` lint, which
is the *memory-safety* half of the stack-wide "Rust-native, memory-safe, one Cargo
workspace per app" principle (caliban-ai shared principles §5). The other half —
that the deliverables themselves are Rust, and that non-Rust consumers are served
over a wire protocol rather than by maintained bindings — was never written down
anywhere. ADR 0007 gets closest, in a framing line noting that the optional daemon
"lets non-Rust tools and remote systems share a store", but it decides transports,
not language scope.

An unrecorded decision is indistinguishable from an unmade one, and this one has
already cost us. The competitor parity evaluation (#193) derives its Notes column
from ADRs. With no ADR to derive from, it ranked **Python/TypeScript client SDKs**
as the single most convergent gap in the entire evaluation — 🔴 in all three
matrices and named in every "Reading of the gap" section — for work the project
had in fact already decided not to do. The matrices have a `by design` convention
precisely to prevent that confusion; it could not be applied to a decision no
document contained. Left alone, every refresh of the evaluation regenerates the
same recommendation.

Two ways to record it were considered:

- **Supersede ADR 0009.** Rejected. 0009 is about workspace layout and the facade;
  language scope for deliverables is a different question, and folding it in would
  churn an accepted ADR whose subject is unchanged — while making the language
  decision harder to cite on its own.
- **A dedicated ADR.** Taken. The decision has its own consequences, its own
  revisit condition, and needs to be citable from a principles entry and from five
  evaluation rows.

## Decision

We will ship **Rust deliverables only**: the crates in this workspace, the
`gonzalod` container image, and prebuilt binaries of the workspace's own binary
crates (ADR 0009 for the workspace; #229 for the binaries).

We will **not** publish or maintain client SDKs in other languages — no Python
package, no npm package, no FFI layer, no generated-and-vendored bindings kept in
this repository.

Non-Rust consumers integrate over **the daemon plus its published schema**: gRPC
or HTTP/JSON against `gonzalod`, generated from the `gonzalo-proto` schema
artifacts that ADR 0007's single-canonical-schema rule already guarantees cannot
drift from the Rust implementation. That boundary is the supported answer, and
generating a client from a published schema is a supported thing to do — it is
maintaining hand-written per-language SDKs that we decline.

This is a scope decision about what *we* deliver, not a restriction on anyone
else. A third party is free to publish a Python or TypeScript client; it simply is
not ours to keep working.

## Consequences

- **Positive:** one implementation to keep correct. Every capability is exercised
  by the conformance suite (ADR 0006) rather than re-implemented per language and
  drifting quietly. The parity matrices can now mark SDK rows `by design` and stop
  re-proposing them. `unsafe_code = "forbid"` stays meaningful, since no FFI
  surface reintroduces what the lint exists to exclude.
- **Negative:** a real adoption cost. A Python user cannot `pip install gonzalo`;
  they must run a daemon and generate a client. That is a heavier first step than
  a library import, and it is the price we are choosing to pay. It also puts
  weight on the schema artifacts actually being published and consumable — the
  daemon boundary is only a credible answer if generating a client from it is
  easy, which is why that work is tracked separately (#198).
- **Revisit if:** a concrete adopter is blocked on the daemon boundary rather than
  merely inconvenienced by it — someone who cannot run a sidecar at all — or if
  schema-artifact publishing proves insufficient to generate a usable client in
  practice. Convergent competitor pressure alone is *not* a reason to revisit;
  that Mem0, Zep and Letta all ship Python SDKs is a fact about their product
  shape, not evidence about ours.
