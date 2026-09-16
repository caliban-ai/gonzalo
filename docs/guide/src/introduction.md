# gonzalo

Gonzalo is the persistence layer for the [caliban-ai](https://github.com/caliban-ai)
stack. It stores versioned records behind one generic `Store` trait, surfaces
concurrent edits as conflicts rather than losing them, and lets the backing
substrate be the local filesystem, a git repository, an S3-compatible object store,
or a remote `gonzalod` daemon, as a matter of configuration.

On top of that core sit capability layers: typed domain views (memory tiers, topics,
sessions, checkpoints), a normalized ticket layer, vector search, a knowledge store,
and a tree-sitter code graph that agents query through an MCP server.

## Where gonzalo fits

| project | role |
|---|---|
| [caliban](https://github.com/caliban-ai/caliban) | the agent. Gonzalo started as a shareable home for its local-first state. |
| [prospero](https://github.com/caliban-ai/prospero) | runs the caliban agent fleet |
| [ariel](https://github.com/caliban-ai/ariel) | the chat bridge for the fleet: Discord first, then Slack and Teams. Its identity, role grants, channel configuration and audit trail are gonzalo records, and ariel stores nothing of its own. Ariel is in early implementation and does not yet read or write gonzalo; the record kinds it needs shipped in 0.7.0 as the [fleet access-control records](./fleet.md) ([ADR 0022](./adr/0022-fleet-access-control-records.md), [ADR 0023](./adr/0023-channel-config-fields.md)). |
| **gonzalo** | persistence: records, stores, capability layers, the daemon, and the code-graph MCP server |

Rust consumers depend on the `gonzalo` facade crate. Everything else talks to the
daemon over HTTP/JSON or gRPC ([ADR 0020](./adr/0020-rust-native-deliverables.md)).

## What you get

| binary / crate | what it is | guide |
|---|---|---|
| `gonzalo` (crate `gonzalo-cli`) | admin CLI: inspect and import records, sync stores, index code graphs, sync tickets | [The CLI](./cli.md) |
| `gonzalod` (crate `gonzalo-server`) | the daemon: a store served over HTTP/JSON and gRPC, with namespace-scoped auth | [Running gonzalod](./daemon.md) |
| `gonzalo-mcp` | MCP server that answers code-graph queries for agents | [The MCP server](./mcp.md) |
| `gonzalo` (library) | facade over the core, substrates and layers, selected by Cargo feature | [Storage backends](./storage.md) |

## Where to go next

- New to the code graph? Start with [The MCP server](./mcp.md). It covers install,
  indexing and registration end to end.
- Deploying the daemon: [Running gonzalod](./daemon.md).
- Picking a substrate, or embedding gonzalo in a Rust program:
  [Storage backends](./storage.md).
- Why things are the way they are: [Guiding Principles & Invariants](./principles.md)
  and the [architecture decisions](./adr/index.md).
- Crate docs: the [API reference](./api/index.html).
