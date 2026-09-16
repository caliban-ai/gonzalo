# The CLI

`gonzalo` is the admin and ops CLI. It works directly against a **filesystem store**
on local disk: a directory, passed as `--root`, that holds records and code-graph
views. It does not talk to a daemon.

Install it as described in [The MCP server § Install](./mcp.md#install). The CLI ships
in the `gonzalo-cli` crate and in the macOS release archive.

`gonzalo --help` and `gonzalo <command> --help` are the authoritative reference. This
page explains what each command is for.

## The store root

Every command that reads or writes a store takes `--root <dir>`, defaulting to the
current directory. A leading `~` is expanded to `$HOME` even when no shell is involved
(a systemd unit, a container `command:`), so `--root ~/.gonzalo` behaves the same
everywhere ([#211](https://github.com/caliban-ai/gonzalo/issues/211)).

Records live under `<root>/<namespace>/<collection>/<id>.json`. Code-graph views live
alongside them. `gonzalo-mcp` reads the same root through `GONZALO_ROOT`.

## Records

| command | does |
|---|---|
| `gonzalo status --root R` | record counts grouped by namespace/collection |
| `gonzalo list --root R [--namespace N] [--collection C]` | list record keys |
| `gonzalo get --root R <namespace> <collection> <id>` | print one record as JSON |
| `gonzalo migrate --root R --namespace N --collection C [--kind K] <src>` | recursively import files from `<src>` as records |
| `gonzalo sync <root-a> <root-b>` | two-way sync of two filesystem stores |
| `gonzalo delete --root R --namespace N --collection C --id I [--expected REV]` | write a tombstone for one record (replicates on sync); `--expected` is the `revision` object from `gonzalo get`'s JSON output (e.g. `{"counter":3,"hash":"…"}`) |
| `gonzalo reset --root R --namespace N [--collection C]` | tombstone every live record in a namespace or collection; idempotent, re-run to finish |
| `gonzalo collect --root R --older-than 30d [--namespace N [--collection C]]` | physically purge tombstones at least that old; choose a horizon longer than any peer's longest gap between syncs |

`get` exits non-zero with a message on stderr when the record is absent, and leaves
stdout empty, so scripts can tell absent from present.

`delete`, `reset`, `collect` and `sync` accept `--ancestor-cap <n>` (default 32). Exit
codes: 0 success, 1 error, 2 usage error, 3 conflict — `delete` with a stale
`--expected`, `reset` leaving records that were edited concurrently (re-run it), or
`sync` reporting conflicts or giving up before it converged (see below).
`collect` reports conflicts but exits 0.

`delete` on a key that's already deleted (or never existed) exits `0` and ignores
`--expected`: to a consumer the key is already absent, so there's nothing to
check the expected revision against. This is the idempotent-delete rule, not a
bug — see [Deleting a record](./deletion.md#deleting-a-record).

`migrate --kind` is one of `topic` (default), `memory-tier`, `session` or
`checkpoint`. It prints how many files it imported and skipped.

`sync` copies records each side is missing, merges records both sides changed, and
reports `copied_to_a`, `copied_to_b`, `fast_forwarded`, `merged`, `conflicts` and
`unconverged`. It also replicates deletions, since tombstones are copied like
records. Merging is keyed by record kind and uses stored ancestry for a 3-way merge
([ADR 0016](./adr/0016-threeway-merge-stored-ancestry.md)). A conflict it cannot
resolve is reported, never silently overwritten.

`unconverged` counts keys whose write was still losing a race when sync gave up
after its pass limit, which happens when writers never stop touching the same keys
during a run. It is normally `0`. When it isn't, **the two stores may still disagree
on those keys even though `conflicts` is `0`**: the run is not proof that they match.
`sync` then prints a line on stderr and exits `3`, so a script can re-run it once the
writers settle rather than treating the run as a success.

## Code graph

| command | does |
|---|---|
| `gonzalo index --root R --repo OWNER/NAME [--view V] <src>` | index a source tree into a view (`--view` defaults to `main`) |
| `gonzalo gc --root R` | sweep code-graph slices no live view references |

`index` flags:

| flag | effect |
|---|---|
| `--watch` | keep running and re-index on filesystem changes until Ctrl-C |
| `--debounce-ms N` | with `--watch`, quiet period after the last change (default 500) |
| `--reconcile-secs N` | with `--watch`, seconds between full reconciles (default 300) |
| `--gc` | after indexing, run a whole-store GC (also applied on each reconcile under `--watch`) |
| `--include PATH` | index a path a built-in rule would skip, such as vendored code. Repeatable. Cannot override `.gitignore`. |
| `--require-parse-worker` | fail instead of falling back to in-process parsing when no `gonzalo-parse-worker` is found |

Slices are content-addressed and shared across views, so deleting a view's source
does not free them. `gc` marks against every live view's manifest across all repos
and reports `manifests`, `freed` and `retained`.

[The MCP server](./mcp.md#index) explains `index` output line by line, which languages
are parsed, and how to keep a view fresh.

## Tickets

Gonzalo can import a ticket board into the store as ticket records, with each card's
column normalized into a state category
([ADR 0010](./adr/0010-ticket-system-capability-layer.md)). Connections are configured
in a TOML file; see `tickets.example.toml` in the repository.

```sh
export KANBAN_PROJECT_PAT=ghp_...        # token named by the connection's token_env
cp tickets.example.toml tickets.toml
gonzalo ticket sync --config tickets.toml --root ./store
gonzalo ticket list --root ./store
gonzalo ticket get  --root ./store --connection caliban-ai-board "caliban-ai/gonzalo#15"
gonzalo ticket move --config tickets.toml "caliban-ai/gonzalo#15" in_progress
```

| command | does |
|---|---|
| `ticket sync [--config F] [--root R] [--author A]` | sync every configured connection into the store. `--config` defaults to `tickets.toml`. |
| `ticket list [--root R]` | list imported ticket keys |
| `ticket get [--root R] [--connection C] <uid>` | show one ticket. Board records are keyed per connection, so pass `--connection` for them ([#159](https://github.com/caliban-ai/gonzalo/issues/159)). |
| `ticket move [--config F] [--connection C] <uid> <category>` | move the card to the column for a category: `triage`, `backlog`, `open`, `in_progress`, `pending`, `done` or `canceled` |

The config file currently accepts one provider, `github-projects` (GitHub Projects v2).
The Jira, Linear, GitLab, Asana and GitHub-issues connectors are available as library
crates behind the facade's `ticket-*` features, but are not wired into `tickets.toml`.
The daemon exposes the same sync as `POST /v1/tickets/sync`; see
[Running gonzalod](./daemon.md#http-api).
