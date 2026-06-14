# ADR 0003 · License: AGPL-3.0-only

- **Status:** accepted
- **Date:** 2026-06-13

## Context

Gonzalo needs a license. It is a persistence layer for caliban, which is itself
licensed **AGPL-3.0-only**, and it is intended to be deployable as a network
daemon (`gonzalod`) — so network-use copyleft is directly relevant rather than
incidental.

## Decision

License gonzalo under **AGPL-3.0-only**, matching caliban. The `LICENSE` file
carries the full text; `license = "AGPL-3.0-only"` is set in the workspace
package metadata.

## Consequences

- **Positive:** Consistent with caliban (the primary consumer); strong copyleft
  including the AGPL network-use provision, which fits a shareable daemon; one
  license story across the sibling repos.
- **Negative:** AGPL deters some commercial/proprietary adopters and cannot be
  linked into closed-source software; operators who modify and serve the daemon
  take on the network-copyleft obligation.
- **Revisit if:** a relicensing need arises (e.g. broader ecosystem adoption),
  which would require consent from all contributors.
