# gonzalo

A robust, shareable persistence layer for [caliban](https://github.com/caliban-ai/caliban).

Gonzalo lifts caliban's local-first state — memory tiers, auto-memory topics,
sessions, and checkpoints — into a layer that can be shared across multiple
systems and contributors, via pluggable storage substrates behind a generic,
versioned, conflict-aware core. See `docs/superpowers/specs/` for the design.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).

## Building

```bash
cargo build --workspace
cargo test  --workspace
```
