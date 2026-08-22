# The MCP server

`gonzalo-mcp` exposes the code graph to an agent over MCP (stdio). An agent can ask
where a symbol is defined, who calls it, what a view contains, and what changed
structurally between two views.

## The two-step model

The single most important thing to know:

> **`gonzalo index` writes. `gonzalo-mcp` only reads.**

The server never indexes anything. Install and register it without indexing and you
get a server that answers every query with "no indexed view" — correctly, but
forever. Indexing is not optional and not discoverable from the server.

```
gonzalo index ──writes──▶  store root  ◀──reads── gonzalo-mcp ◀── agent
                          (~/.gonzalo)
```

A **view** is one indexed snapshot of one repo, addressed by `(repo, view_id)`.
`repo` is any identifier you choose (by convention `owner/name`); `view_id` is
typically a branch. Multiple repos and multiple views share one store root cleanly —
each gets its own SQLite graph under `<root>/graphs/<encoded-repo>/<view>.db`.

## Install

```sh
cargo install gonzalo-cli gonzalo-mcp gonzalo-parse
```

`gonzalo-parse` supplies `gonzalo-parse-worker`, which isolates tree-sitter parsing in
a subprocess so one bad file skips instead of aborting the whole index. Indexing works
without it — it silently falls back to in-process parsing — so install it explicitly
rather than assuming it is there.

Cargo installs to `~/.cargo/bin`. That directory is on `PATH` for *interactive* shells
via `~/.zshrc`, but an MCP client typically spawns a **non-interactive** shell, which
reads `~/.zshenv` instead. If the server fails to start, this is usually why: either
put the directory on `PATH` in `~/.zshenv`, or register the server with an absolute
path to the binary.

## Index

```sh
gonzalo index --root /Users/you/.gonzalo --repo acme/widgets --view main /path/to/checkout
```

Output tells you what happened:

```
driver:   full walk
files:    465
added:    465
modified: 0
deleted:  0
skipped:  0
ignored:  2 files, 9 dirs not descended
```

- `driver` — `full walk` the first time; afterwards a git-diff-driven **incremental**
  pass that re-parses only what changed. A no-op re-index is well under a second.
- `skipped` — files a parse worker crashed or hung on.
- `driver` also goes back to `full walk` on its own when gonzalo's extraction format
  changes, so a parser improvement reaches files that did not themselves change.
- `ignored` — files and directories deliberately left out of the view: dependency and
  build-output directories, generated bundles (`*.min.js` and friends), and anything
  `.gitignore`d. Use `--include <path>` to re-admit a vendored path you *do* want
  indexed. `--include` cannot override `.gitignore`, because a view must stay
  reproducible from the commit alone.

### Keeping a view fresh

Views are snapshots. Re-index after checking out or pulling — it is incremental:

```sh
gonzalo index --root /Users/you/.gonzalo --repo acme/widgets --view main /path/to/checkout
```

`gonzalo index --watch` keeps a view current continuously, with a debounce and a
periodic full reconcile.

Because views are per-`(repo, view_id)`, git worktrees pair naturally with them: index
each worktree under its own `view_id` and `diff` them.

## Register

```sh
claude mcp add gonzalo --env GONZALO_ROOT=/Users/you/.gonzalo -- gonzalo-mcp
```

> **Use an absolute path.** `GONZALO_ROOT` is passed straight through, and nothing in
> gonzalo expands a leading `~`. Neither bash nor zsh expands a tilde in
> `--env KEY=~/path` argument position either, so `GONZALO_ROOT=~/.gonzalo` creates a
> directory *literally named* `~` in the current working directory and indexes
> nothing you can find. See [#211](https://github.com/caliban-ai/gonzalo/issues/211).

Restarting or reconnecting the MCP client respawns the server process, which is how a
newly installed binary's tools become visible — a full client restart is not needed.

## Verify

```
status  →  {"status":"ok","root":"/Users/you/.gonzalo","views":2}
views   →  [{"repo":"acme/widgets","view_id":"main","files":465,"base_commit":"9ea0860…"}]
```

`status` reports the number of indexed views, so a server pointed at an empty or wrong
store is visibly wrong rather than merely "ok". `views` lists the valid `(repo,
view_id)` pairs — call it first rather than guessing a selector.

`base_commit` is the commit the view was last indexed at. Compare it against
`git rev-parse HEAD` to detect a stale view, which is the quiet failure mode: results
that are plausible but describe code that has moved on.

### A selector error is an error

A query naming a view that does not exist returns a tool **error** that names the
selector and lists the views that do exist:

```
no indexed view 'acme/widgets/mian'. This is a selector error, not an empty result.
Indexed views: acme/widgets/main. Call `views` to list them.
```

An empty result therefore means what it says: the symbol is not in that view. This was
not always true — see [#210](https://github.com/caliban-ai/gonzalo/issues/210) — and it
matters because an agent reads `[]` as "nothing calls this" and reports it as fact.

## The tools

**Discovery**

| tool | answers |
|---|---|
| `views` | which `(repo, view_id)` pairs exist, with file counts and indexed commit |
| `status` | is the server up, which root, how many views |

**Whole-view** — start here in an unfamiliar repo; none need a symbol name.

| tool | answers |
|---|---|
| `overview` | counts, breakdown by kind and language, largest files |
| `top` | most referenced (`fan_in`), most calls out (`fan_out`), most ambiguous (`definitions`) |
| `list` | enumerate symbols filtered by path prefix, kind, name substring |
| `unreferenced` | dead-code *candidates* — heuristic, see below |

**Per-symbol** — for a name you already have.

| tool | answers |
|---|---|
| `search` | where is this defined |
| `node` | definitions + callers + callees in one call |
| `callers` / `callees` | who calls this / what does this call |
| `explore` | every reference, with paths |
| `impact` | transitive caller closure, resolution-gated; takes `max_depth` |

**Across views**

| tool | answers |
|---|---|
| `diff` | symbols and references added/removed from `view_a` to `view_b` |

`diff` is the most useful tool here for reviewing work: point it at a branch view and
a main view and it reports what changed *structurally*, which is a different and often
better question than what a textual diff shows.

Every whole-view result is bounded and reports `total` and `truncated`, so a capped
list never masquerades as a complete one.

## Capability boundaries

Read this before trusting a result.

**The call graph is name-matched, not type-resolved.** Two unrelated functions sharing
a name are one node. `callees` includes enum variants, constructors, and std methods
(`Some`, `ok`, `from`) alongside project functions. `top by=definitions` is the
ambiguity report: any name scoring above 1 is defined in several places, and every
traversal through it merges unrelated subgraphs.

**`impact` follows only resolvable edges.** It used to walk the name-matched graph and
return roughly half the repository for one seed; it now keys on `(name, defining path)`
and refuses to traverse an ambiguous reference, reporting the count in
`ambiguous_edges` instead ([#207](https://github.com/caliban-ai/gonzalo/issues/207)).
Read that count: non-zero means the true set may be larger than what you got. Method calls
(`x.foo()`) on a receiver whose type is unknown are not attributed at all and are
counted in `receiver_unknown_edges`, which stops a std method being credited to a
same-named project function
([#223](https://github.com/caliban-ai/gonzalo/issues/223)). Treat the closure as a lead
list and confirm the load-bearing edges with `callers`.

**`callers` on a type is always empty.** Only call expressions are edges, so types,
traits, and structs have no inbound edges. Empty there means "not applicable", not
"unused".

**`unreferenced` is a heuristic.** A function used only as a value — `and_then(f)`,
`map_err(be)` — is a path expression rather than a call, so it records no reference and
will be reported as dead when it is not. An unused name is also hidden by any
same-named symbol that *is* used. Confirm every hit against the source.

**Calls inside Rust macro arguments are recorded**, including in `assert_eq!` and
`println!`, but by a token-level rule: an identifier followed by a parenthesised token
tree. That is deliberately over- rather than under-inclusive — a tuple-struct pattern
like `Some(_)` reads as a call. In C and C++, calls inside a `#define` *body* are still
invisible, because the body is a single opaque preprocessor token.

**Test functions are ordinary symbols** and rank alongside production code. Check the
path before concluding something is only used in tests.

**Generated and vendored code is excluded by default**, along with `.gitignore`d build
output. If a query returns nothing you expected from a vendored path, that is why —
re-index with `--include <path>`.

## Troubleshooting

| symptom | cause |
|---|---|
| server will not start | `~/.cargo/bin` not on `PATH` for non-interactive shells — set it in `~/.zshenv` or register an absolute path |
| `status` shows `"views":0` | nothing indexed yet, or `GONZALO_ROOT` points somewhere unexpected — check the `root` it reports |
| a directory named `~` appeared | `GONZALO_ROOT=~/…` was not expanded; use an absolute path |
| "no indexed view" error | the selector does not match — call `views` |
| results describe code that changed | stale view — compare `base_commit` to HEAD and re-index |
| new tools missing after an upgrade | the client is still running the old binary — reconnect the MCP server |
