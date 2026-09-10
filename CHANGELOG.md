# Changelog

All notable changes to gonzalo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, the minor version is bumped for new features and
the patch version for fixes.

## [Unreleased]

### Changed

- **`callers` and `callees` now report how ambiguous the queried name is.** Both
  are raw name matches, so a name defined in several places returns the merged
  answer for all of them — and a bare list said nothing about that, which is the
  point where a heuristic result is most easily read as a precise one. Each now
  answers with a `definition_count` alongside the list, plus `defined_in` while
  there are few enough paths to be a lead rather than noise. A count of 0 means
  the name is defined nowhere in the view, which is what makes an empty list
  readable rather than ambiguous between "nothing calls this" and "you asked the
  wrong question".

  This changes the shape of those two tool results: the list moved under a
  `callers`/`callees` key, so they return an object rather than a bare array.
  `impact` already reported the edges it declined; this brings the same honesty
  to the tools that do no resolution at all. (#249)

### Fixed

- **`unreferenced` no longer reports live callbacks as dead code.** A function
  passed as a value rather than called — `and_then(helper)`, `register(handler)`
  — is a path expression, not a call expression, so it recorded no reference at
  all and the function looked unused. That was the documented main false
  positive of the one tool whose suggested action is deletion. On gonzalo's own
  source it was flagging 98 symbols that are genuinely used, among them the test
  fixtures handed to `build_rust` and constants passed to functions.

  Such a reference is now recorded with its own kind and deliberately kept out of
  the call graph: `callers`, `callees` and `top` stay call-only, because passing
  a function is not calling it. `impact` counts them in a new `value_edges`
  rather than walking them — a callback is a real dependency, so dropping it
  silently would under-report, but extraction is per-file and cannot tell an
  identifier naming a function from one naming a local, so traversing them would
  put false edges back into the walk #207 and #223 cleaned up. Non-zero
  `value_edges` means go look.

  The over-inclusion is deliberate and one-directional: a local variable handed
  to a call is recorded too, which for dead-code detection errs towards *not*
  calling something dead. `EXTRACTION_VERSION` is 4, so each view re-walks once
  on upgrade. (#250)

### Added

- **Qualified symbol identity in the code graph.** A call site's path qualifier
  and a definition's owning type are now both recorded, so `Beta::get()` is
  attributed to `Beta`'s `get` instead of merging with every other `get` in the
  view. Two pieces of disambiguating information were sitting in the syntax tree
  and being discarded: a qualified callee was reduced to its trailing identifier,
  and the walk tracked only the enclosing *function*, so a method in an `impl`
  block was stored as a bare name with nothing recording what it hangs off. The
  qualifier only ever narrows — when it names something the view does not own,
  such as a file module or a dependency, it is ignored and the previous rules
  apply, so no edge that resolved before stops resolving now. Recorded where a
  grammar makes a path unambiguously a path (Rust, C++, PHP); Go's
  `pkg.Func()` and Java's `Foo.bar()` cannot be told from a receiver call at
  this level and stay unqualified rather than record a variable as a type.
  `EXTRACTION_VERSION` is 3, so each view re-walks once on upgrade. (#248)

- Prebuilt macOS Apple Silicon binaries on every tagged release. A `v*` tag now
  also builds `gonzalo-vX.Y.Z-aarch64-apple-darwin.tar.gz` (plus a `.sha256`) and
  attaches it to the GitHub Release. The archive holds `gonzalo`, `gonzalo-mcp`
  and `gonzalo-parse-worker` together, which is the point: installed separately
  from crates.io those three can land at different versions, and a CLI newer than
  its parse worker indexes with the old parser and then marks the view current
  (#212, #228). `aarch64-apple-darwin` is the only target built; the container
  image covers Linux. See `docs/releasing.md`. (#229)

### Changed

- `ParseMode` and `WorkerSource` are `#[non_exhaustive]`. They ship for the
  first time this release, and the set of ways a worker can be located is
  already known to be growing, so sealing them now avoids a breaking change
  later. (#241)

- Recorded the **Rust-native deliverables** principle as ADR 0020: gonzalo ships
  Rust crates, the container image and its own binaries — not client SDKs in
  other languages — and non-Rust consumers integrate over the daemon plus its
  published schema. The decision was real but written down nowhere, so the
  competitor parity evaluation kept ranking Python/TS SDKs as the single most
  convergent gap in the whole evaluation. All five SDK rows are now marked
  `by design` with the citation. (#196)

- `docs/releasing.md` no longer claims version bumps are exempt from rate
  limiting. crates.io enforces a second limit on updates to existing crates, and
  a 24-crate workspace trips it on every release — `v0.5.0` stopped after 23 of
  24. The expected 429-and-resume flow, and a check that every crate is live
  before creating the Release, are now documented. (#227)

- Documented that the gRPC decode ceiling is per-server, not per-method: raising
  `GONZALO_MAX_BLOB_SIZE` also raises the decode limit for record RPCs, which
  HTTP does not do. Bounded and authenticated, so documented rather than
  closed. (#194)

### Fixed

- The packaged `gonzalo` binary is portable. It linked **Homebrew's** libgit2 at
  `/opt/homebrew/opt/libgit2/…`, an absolute path in the Mach-O load command, so
  on any Mac without Homebrew it died in dyld before executing an instruction —
  not even `--version` ran. `gonzalo-mcp` and `gonzalo-parse-worker` were always
  clean, so the archive shipped one broken binary out of three, and it was the
  one that creates the view the MCP server reads. libgit2 is now vendored and
  statically linked, matching what the Linux container already did by accident
  of having no `libgit2-dev` installed. `scripts/package-macos.sh` now refuses
  to package a binary that links anything outside `/usr/lib` and
  `/System/Library`, because which way this went depended on whether the build
  machine happened to have a system libgit2. (#246)

- Ticket connectors keep the provider's error message instead of discarding it.
  `SourceError::Backend` is documented as carrying the provider's message, but
  all five connectors reached it through reqwest's `error_for_status()`, which
  drops the response body — so an expired token, a missing scope, a malformed
  JQL query or a rate-limit hint all surfaced as a generic "HTTP status client
  error". Twenty call sites across Jira, GitHub, GitLab, Asana and Linear now
  report the status and the provider's own reason, bounded in length. (#240)

- `gonzalo sync` expands a leading `~` in its two store roots. Those are
  positional arguments and were the one place #211's expansion did not reach,
  so `gonzalo sync '~/a' '~/b'` from a non-shell context silently operated on
  two directories that did not exist and reported success. (#238)

- The parse-worker version note no longer claims correct data is wrong, and no
  longer repeats. A worker released before `--extraction-version` cannot report
  one while still producing current extraction, so that case now reads as an
  unconfirmed note rather than a warning to go fix your data; a worker that
  reports a *different* version still warns plainly. It is also emitted once per
  process rather than once per index, which under `--watch` meant once per file
  save. (#239)

- Remote reads report failures the way remote writes do. `ServerStore`'s read
  paths used reqwest's `error_for_status()`, which discards the response body,
  so a `403` on a read surfaced as a generic HTTP status line while the same
  refusal on a write reported the daemon's reason. All four HTTP read paths now
  carry status and body; `404` still means `Ok(None)` on `get`/`get_blob`. (#195)

- A leading `~` in a store root is expanded to `$HOME` instead of being taken
  literally. `GONZALO_ROOT=~/.gonzalo` in an MCP client config reaches the
  process verbatim — there is no shell in that path — so it created a directory
  *literally named* `~` in whatever the working directory happened to be, and
  every query then answered from the wrong, empty store. The form users
  naturally write, and the one that works when tried in a shell, was exactly the
  form that silently misbehaved where it mattered. Expansion covers
  `GONZALO_ROOT` and every `--root` argument, which has the same exposure from a
  systemd unit or container spec, and `status` now reports the expanded path so
  it is verifiable. Only a leading `~` is touched: `~otheruser/…` is left alone,
  and a `~` anywhere but the front is an ordinary directory name. (#211)

- `gonzalo index` says which parse mode it is using instead of silently
  degrading. Crash isolation depends on finding `gonzalo-parse-worker`, and when
  it could not, indexing quietly parsed in-process — no log line, no warning, no
  way to tell. The two modes produce byte-identical graphs, so the loss was
  invisible until a tree-sitter grammar aborted and took down the whole run
  instead of skipping one file. A `parse:` line now names the worker and how it
  was found; the fallback prints a warning listing every location searched; and
  `--require-parse-worker` turns a missing worker into a hard error for CI and
  container builds. Lookup also covers `PATH`, so symlink and split-install
  layouts — where a sibling check cannot succeed but the worker is plainly
  available — find it. (#212)

- A stale `gonzalo-parse-worker` no longer produces pre-upgrade extraction that
  the view records as current. `EXTRACTION_VERSION` guarded the CLI, but the
  worker is a separately installed binary and is what actually parses — so a
  0.5.0 CLI driving a 0.4.0 worker emitted old-format slices and then stamped
  the view with the new version, leaving it wrong *and* marked up to date, which
  no later re-index would repair. The worker now answers
  `--extraction-version`, the indexer asks before parsing, and a view is
  credited to the version the parse path actually produced. A mismatch warns,
  rebuilds in full, and self-heals once the worker is upgraded. A worker too old
  to answer is recorded as unknown rather than assumed to agree. (#228)


## [0.5.0] - 2026-08-22

Code-graph correctness. `impact` and reference resolution stop asserting edges
they cannot justify — a closure that returned half the repo for one seed, and a
name-matched resolver that credited std methods to same-named project functions.
Alongside that, the MCP server gains aggregate queries that answer questions about
a whole view rather than about a symbol name the caller already has.

**Upgrading:** the first index after this release does a one-time **full** re-walk
of every view (see `EXTRACTION_VERSION` below) — expected, not a fault. Reinstall
the binary as well as re-indexing: the resolution fixes live in the query path, so
a stale `gonzalo-mcp` keeps returning the old answers over freshly indexed data.

### Fixed

- **A method call no longer claims a same-named free function** (#223).
  `Resolution::UniqueGlobal` attributed a reference to the sole definition of
  that name in the view — including when the name was really a std or dependency
  *method*. In gonzalo, `.chain(ours_obj.keys())` in `gonzalo-core` resolved to
  `fn chain()`, a test fixture in `gonzalo-graph`, a crate `gonzalo-core` does
  not depend on.

  References now record the shape of the call site (`RefKind::{Free, Method}`),
  and a cross-file method call resolves to the new
  `Resolution::ReceiverUnknown` rather than guessing. Measured over gonzalo's
  own source: of the 388 cross-file method calls that used to resolve
  `UniqueGlobal`, **294 pointed at a different crate** — `push`, `filter`,
  `send`, `bytes`, `next` and friends. Same-file method calls still resolve
  `Local`, and free and path calls (`foo()`, `a::b::foo()`) are unaffected.

  Effect on `impact` (#207), same 122-file source both runs:

  | seed | before | after |
  |---|---:|---:|
  | `build_rust` | 185 | **126** |
  | `assemble` | 60 | **23** |
  | `resolve_references_to` | 6 | 6 |

  The 24 provably-false `gonzalo-core` nodes in the `build_rust` closure are now
  **0**. Dropped edges are reported as `receiver_unknown_edges`, counted
  separately from `ambiguous_edges` because the cause differs: not "too many
  candidates" but "cannot claim any candidate".

  `RefKind` is omitted from the serialized slice when free, so a file of plain
  calls keeps its existing content hash.

- **`impact` no longer merges unrelated code through shared identifiers** (#207).
  The closure walked the name-matched caller graph, so one hop into a name with
  several definitions absorbed every subgraph sharing that identifier. The walk
  now keys nodes on `(name, defining path)` and consults the resolver for every
  edge: an `Ambiguous` reference is counted and dropped rather than traversed.

  On the gonzalo view, seeded at `build_rust`: **356 → 178** reached names, with
  10 ambiguous edges reported rather than followed. Seeds that were already sharp
  are unchanged (`resolve_references_to`: 5 → 5).

  The result is now a report rather than a name list — every node carries the path
  defining it, `ambiguous_edges` says how many edges could not be attributed (so a
  non-zero count means the true set may be larger), and `truncated` reports a walk
  stopped by the new optional `max_depth`. The daemon's HTTP and gRPC transports
  keep their existing name-list shape and so get the precision fix without the
  report fields.

  Of the remaining 178, 17 are still provably false and trace to a single
  `UniqueGlobal` over-attribution — std's `Iterator::chain` resolving to a
  same-named test fixture. That is a distinct defect, filed as #223.

- **An incremental re-index now prunes paths a laxer run admitted** (#209
  follow-up). The filter added in #218 only applied to newly walked or changed
  files, so an *existing* view kept its vendored bundles forever: a bundle never
  changes, so it never appears in the git diff and was never reconsidered — and
  once a base commit is recorded there is no full walk to clean it up. Upgrading
  therefore fixed new views only, which is the case least in need of fixing.

  The carried-forward set is now re-checked against the current rules, including
  `.gitignore` — necessary because `docs/guide/book/` is build output excluded by
  ignore rules rather than by any directory-name rule, so a path-only prune left
  it behind.

  Re-indexing the existing `caliban-ai/caliban` view: 17 162 symbols → **8 549**,
  with vendored symbols going 8 618 → **0** and the largest file becoming
  `caliban/src/tui/events.rs` (142) instead of a copy of `mermaid.min.js`.

- **An unknown `repo`/`view_id` is now an error, not an empty result** (#210).
  Every graph query returned `[]` with `isError: false` when the selector named
  no indexed view, so a one-character typo in `view_id` was indistinguishable
  from a symbol that genuinely is not there — an agent read it as "nothing calls
  this" and reported a wrong answer as fact. `Service::view` now fails with
  `NotFound`, and the MCP layer turns that into a tool error naming the
  unresolved selector *and* listing the views that do exist, so a caller can
  correct itself in one round trip. A real miss inside a real view still returns
  `[]`, so the two cases are finally distinguishable.

  `diff` gets the same check on both `view_a` and `view_b`.

- **Calls inside Rust macro arguments are now recorded as references** (#216).
  Macro arguments parse as a `token_tree` of raw tokens rather than expressions,
  so `assert_eq!(f(), 1)` contained no `call_expression` and the call to `f` was
  never seen. Because assertions are where much of a codebase is exercised, this
  silently removed a large share of the call graph: `callers`, `callees`,
  `impact` and `top by=fan_in` all undercounted, and `unreferenced` reported
  live functions as dead.

  Re-indexing gonzalo itself, with an identical file set (1 852 symbols both
  runs), references go from 10 247 to **12 196 — +1 949 edges, +19.0%**.
  `Language::from_extension`, the symbol that exposed the bug, goes from 0
  recorded references to 28.

  Detection is token-level: an identifier whose immediate next sibling is a
  parenthesised token tree. A nested macro has a `!` between the two and is
  excluded. It is deliberately over- rather than under-inclusive — a tuple-struct
  pattern like `Some(_)` reads as a call — which matches a graph that already
  records constructors and enum variants as calls.

  The other 17 grammars were audited for the same opaque-node hole. Only C/C++
  has one: a `#define` body is a single opaque `preproc_arg` token with no child
  nodes to read. It is left in place and pinned by a test so the gap is
  discoverable rather than silent.

- **The indexer no longer walks vendored bundles or gitignored build output**
  (#209). `is_indexable` skipped only `target`, `.git`, and dotted components, so
  half of a real repo's graph was not that repo's code. Membership now lives in
  one place (`IndexFilter`), shared by the full walk and the git-incremental
  driver so they cannot disagree: dependency/output directories (`node_modules`,
  `vendor`, `dist`, `build`, `site-packages`, `third_party`) and generated files
  (`*.min.js`, `*.min.css`, `*.bundle.js`, `*-lock.json`) are dropped on both
  paths, and the full walk additionally honours `.gitignore`.

  Re-indexing `caliban-ai/caliban` drops it from 16 986 symbols to 8 501 (-50.0%)
  with **zero** symbols from `book/**` or any `*.min.js`; the largest file in the
  view is now `caliban/src/tui/events.rs` (142 symbols) rather than a 4 231-symbol
  copy of `mermaid.min.js`. This also removes a reproducibility hole — indexing
  gitignored output made the graph depend on whether anyone had run a build.

  `gonzalo index` now reports what it excluded (`ignored: N files, M dirs not
  descended`), and `--include <path>` re-admits a vendored path that a built-in
  rule would drop. `--include` deliberately cannot override `.gitignore`, so no
  flag can make a view irreproducible.

### Added

- **`EXTRACTION_VERSION`, and a full walk when it changes** (#223). The
  incremental driver carries unchanged slices forward untouched, so a parser
  improvement never reached files that did not change — an existing view stayed
  permanently half-upgraded. `gonzalo index` now records the extraction format
  alongside the view and rebuilds in full when it differs, which is what lets
  #216's and #223's parsing changes actually reach an established view.

- **A guide chapter for the MCP server** (#208) — `docs/guide/src/mcp.md`, covering
  install → index → register → verify → keep fresh, a tool reference grouped by the
  question each tool answers, and the capability boundaries. It leads with the thing
  nothing in the repo stated: the server only *reads*, `gonzalo index` writes, so an
  unindexed setup answers every query forever with no indication why. It also records
  the traps found while wiring the server up for real — `GONZALO_ROOT` not expanding
  `~` (#211), `~/.cargo/bin` missing from the non-interactive shells an MCP client
  spawns, and needing to reconnect the server to pick up a newly installed binary —
  plus a troubleshooting table keyed by symptom.

- **`views` discovery tool and a view count in `status`** (#210). `views` lists
  every indexed `(repo, view_id)` with its file count and the commit it was
  indexed at, which makes the server self-describing rather than dependent on
  out-of-band documentation; comparing `base_commit` against the checkout's HEAD
  also surfaces a stale view, the quieter form of the same problem. `status` now
  reports how many views are indexed, so the natural health-check call actually
  detects a server pointed at an empty or wrong store. The `repo`/`view_id`
  schema descriptions now say they must match an indexed view and point at
  `views`.

- **Aggregate code-graph queries** (#214) — three MCP tools that answer questions
  about a view rather than about a symbol name the caller already has:
  `overview` (file/symbol/reference counts, a breakdown by kind and language, and
  the largest files), `top` (rank by `fan_in`, `fan_out`, or `definitions` — a
  `definitions` score above 1 marks an ambiguous name), and `list` (enumerate
  symbols filtered by `path_prefix`, `kind`, and `name_contains`). Backed by new
  default `GraphStore` methods, so every store implementation inherits them.
  Results are bounded and report `total` + `truncated` rather than silently
  cutting.

- **`unreferenced` dead-code candidates** (#214) — a fourth aggregate tool
  listing symbols with no inbound reference, filtered by the same
  `path_prefix`/`kind`/`name_contains` and bounded the same way. `exclude_tests`
  (default on) drops members of a `mod tests`/`mod test` block by line range and
  anything under a `tests/` directory; on gonzalo itself that is the difference
  between 515 hits and 40. Deliberately errs toward silence — a reference from
  anywhere counts, including from tests and from the symbol itself. Its false
  positives are documented in the tool description, the rustdoc, and a pinned
  test: calls inside macro arguments are not recorded at all (`assert_eq!(f(),
  1)` registers nothing), and a function passed as a value is a path expression
  rather than a call, so both look uncalled.

## [0.4.0] - 2026-08-01

The remote-parity & backend-qualification release. Deletion and blobs — the two
gaps that kept the daemon substrate behind `fs`/`s3` — close, so a daemon-backed
consumer now gets the full `Store` + `BlobStore` surface. Alongside them, an HA
soak harness doubles as a conditional-write qualifier for S3 backends, and its
first finding disqualifies Garage outright.

### Added

- **`Store::delete`** (#183) — OCC-aware record deletion across every substrate
  and the daemon, propagated by `Sync`. Local-only semantics; see ADR 0018.
  (#185)
- **Blobs over the daemon** (#184) — the content-addressed `BlobStore` is exposed
  on `gonzalo-server` (HTTP `GET|PUT|DELETE /v1/blobs/{hash}`, `GET /v1/blobs`,
  plus gRPC), and `ServerStore` implements `BlobStore` over both transports, so a
  daemon-backed consumer gets the full `Store` + `BlobStore` surface. Blobs
  previously worked only on `fs`/`s3`. Adds the `GONZALO_MAX_BLOB_SIZE` daemon
  knob (default 64 MiB). (#192)

### Changed

- **S3 backends are now qualified, and Garage is not among them** (#52) — atomic
  `If-Match` is a hard requirement for any S3-compatible backend. Garage does not
  provide it: gonzalo's conditional-write conformance case expects exactly 1 of 8
  concurrent racers to commit, and Garage let 8/8 through on v1.0.1 and 3–8
  through non-deterministically on v2.1.0 — the signature of a check-then-set,
  not an atomic CAS. **Deployments running gonzalo over Garage can silently lose
  concurrent writes.** RustFS (Apache-2.0) is the qualified backend; MinIO passes
  the qualifier but is rejected on project sustainability. See ADR 0019. (#186,
  #205)

Testing: an HA soak harness for stateless `gonzalod` replicas over an
S3-compatible store — backend-agnostic, doubling as the conditional-write
qualifier above (#52, #186); cross-crate integration tests extracted into
`gonzalo-integration-tests` (#190, #191).

Project: a crates.io publishing pipeline for the workspace, triggered on `v*`
tags (#187, #189).

Docs: competitor capability inventories and parity-gap matrices for mem0, Zep,
and Letta under `docs/evaluation/` (#193); ADR 0019 recording the S3 backend
qualification (#205).

## [0.3.0] - 2026-07-11

The hardening & language-breadth release. A broad correctness and robustness
sweep — a 20-finding QA pass turned into fixes across the core merge/OCC model,
every storage substrate, the daemon, the ticket connectors, and the
knowledge/vector layer — lands alongside eight new code-graph grammars that
take language coverage from nine to **seventeen**.

### Added

- **Language breadth for the code graph** — grammars for **Ruby, PHP, Bash**
  (#87), **Kotlin** (#87), **Swift** (#87), **Lua** (#87), **Scala** (#87), and
  **Elixir** (#87). Elixir is homoiconic (`def`/`defp`/`defmacro`/`defmodule`
  parse as ordinary `call` nodes), so it uses a value-based `walk()` dispatch on
  the call target's text rather than a node-kind mapping. Coverage is now 17
  languages. (#126, #127, #128, #129, #130, #181)

### Changed

- **Ticket `RecordKey`s are board-scoped** — a board-scoped source folds its
  connection/board discriminator into the key, so the same issue imported from
  two boards no longer collides onto one thrashing record. Stored keys for board
  sources change shape and re-import on the next sync. (#159)

### Fixed

- **core** — record-key encoding is now a reversible, injective percent-style
  codec, so distinct keys can never collide onto one physical path (silent
  cross-key overwrite / OCC bypass); append-only merge preserves blank and
  legitimately-repeated committed lines instead of stripping/de-duping them, and
  the `Derived`/gc semantics are corrected. (#131, #133)
- **storage substrates** — the git substrate locks the `put` critical section
  (no lost updates under concurrent writers) and detects non-fast-forward push
  rejection; filesystem writes fsync the temp file and parent directory for
  crash durability; S3 list-pagination terminates when the continuation token is
  absent; graph-sqlite view-db paths use the injective encoder. (#132, #134,
  #144, #145)
- **daemon** — authorization runs before request deserialization, internal
  backend errors are returned opaquely (no path/bucket/SQLite leakage), the PUT
  record route validates its URL path against the body key, and the remote
  client surfaces daemon 403/413 responses instead of masking them as a decode
  error. (#146, #147)
- **code graph** — JS/TS arrow-function and function-expression bindings are
  extracted, PHP method and static calls are recorded, and Swift/Kotlin
  `struct`/`enum`/`interface` declarations get their correct `SymbolKind`. (#136)
- **knowledge / vector / domain** — a corrupt knowledge-bearing body surfaces an
  ingest error (rather than silently not indexing) and removed records are
  de-indexed; non-finite vectors score `0.0` and rank deterministically; the
  domain codec rejects a `Body::Blob` instead of misparsing its content hash.
  (#139, #149, #154)
- **cli** — `get`/`ticket get` exit non-zero (message on stderr) when a record
  is absent, `index` advances the persistent SQLite graph only after the
  manifest commits, and `--gc` is honored under `--watch`. (#152)
- **ticket connectors** — Jira routes a `Canceled` move to a won't-do status
  rather than Done; a closed GitLab issue is terminal and non-terminal moves no
  longer report a false success; Linear fails a mutation that returns
  `success: false`; the GitHub REST connector follows `Link` pagination so all
  issues import, not just the first 100. (#138, #140, #141, #142)

Testing: de-flaked `gonzalo-parse`'s hung-worker timeout test, whose 300 ms
budget false-timed-out the healthy recovery parse under heavy build load. (#178)

## [0.2.0] - 2026-07-06

The code-graph release. Gonzalo grows a full **code-graph capability** —
tree-sitter parsing across nine languages, content-addressed slices with
two-level keying, a persistent SQLite `GraphStore`, and structural queries over
both the daemon and an MCP server — alongside real semantic search (a local
Candle embedder feeding an approximate ANN index over per-kind-chunked
knowledge), namespace-scoped daemon auth, and correct content-aware 3-way merges
on both store-sync and git pull. Distributed as the `gonzalod` container image.

### Added

- **Code-graph capability** (EPIC A–K): tree-sitter parsing into path-agnostic,
  content-addressed slices with two-level keying (content+grammar hash for
  storage, per-view manifest for resolution); a persistent `SqliteGraphStore`;
  assembly-time name resolution; and structural queries — definitions,
  references, callers, callees, transitive impact, and cross-view diff — served
  over the daemon and a dedicated **code-graph MCP server**. Parsing is
  crash-isolated behind a worker-subprocess `ParserPool`. (ADR 0012;
  #48, #50, #54, #56, #61, #64, #66, #70, #71, #74, #77, #88, #89, #90, #10, #30)
- **Language breadth for the code graph** — a `Language` dispatch with grammars
  for Rust, Python, JavaScript/TypeScript/TSX, Go, Java, C#, C, and C++. (#79,
  #81, #83, #84, #85, #86)
- **CLI indexing** (`gonzalo index`): index a source tree into a code-graph view,
  git-diff-driven incremental re-sync, a `--watch` file-watcher for live
  re-index, and opt-in mark-sweep GC of unreferenced slices. (#74, #93, #94,
  #100, #104)
- **Knowledge store** (`gonzalo-knowledge`): a "what do we know about X" surface
  composing `Store` + `VectorIndex` + `Embedder`, a **vector⋈graph** join
  (semantically similar *and* structurally near), and **per-kind chunking** so
  long records retrieve at turn/section granularity. (ADR 0011; #30, #29)
- **Real semantic vector search**: `gonzalo-embed` — a local CPU sentence
  embedder (Candle + all-MiniLM-L6-v2) — and `HnswVectorIndex`, an approximate
  ANN backend, behind the existing `Embedder`/`VectorIndex` traits.
  (ADR 0013, ADR 0014; #97, #9)
- **Daemon substrate selection & health**: env-driven `fs|s3` store selection
  with a native S3 `BlobStore`, and unauthenticated `/healthz` + `/readyz`
  probes for k8s. (#62, #63)
- **Namespace-scoped daemon auth**: a token→principal model with per-namespace
  read/write scoping enforced on both transports, plus unforgeable author
  stamping. (ADR 0015; #11)
- **`gonzalod` container image** — the release artifact, published on a `v*` tag.
  (#51, #65)

### Changed

- **Store sync — true 3-way merge**: `AncestryStore` retains each version's body
  by revision hash so `sync` can merge divergent structured records against their
  real common ancestor instead of an empty base. (ADR 0016; #2)
- **Git pull — content-aware non-fast-forward merge**: a diverged pull now
  reconciles per-record through gonzalo's class-aware `merge()` into a two-parent
  merge commit, surfacing unresolved records instead of erroring. (ADR 0017; #7)
- **S3 native conditional writes**: `If-Match`/`If-None-Match` close the
  optimistic-concurrency TOCTOU window in the S3 substrate. (#5)

### Fixed

- CI: serialize GitHub Pages deploys with a concurrency group. (#107)

### Internal

- ADRs 0010–0017 added (ticket capability layer, two-level code-graph keying,
  local embedder, ANN backend, namespace auth, stored-ancestry 3-way merge,
  content-aware non-FF pull).
- Docs: an mdBook guide publishing the ADR log, changelog, and a synthesized
  **Guiding Principles & Invariants** page; README status badges. (#103, #38)

## [0.1.0] - 2026-07-03

The initial development line — a generic, versioned, conflict-aware persistence
layer for [caliban](https://github.com/caliban-ai/caliban), built milestone by
milestone (M1–M6).

### Added

- **Record/Store core** (M1): `gonzalo-core` — one uniform `Record` model and a
  generic `Store` trait, with revisions, optimistic-concurrency `parent`
  tracking, `PutResult::Conflict`, per-`RecordKind` merge, and a feature-gated
  substrate **conformance suite**. No I/O in the core. (ADR 0002, ADR 0005,
  ADR 0006)
- **Filesystem substrate + domain + facade** (M1): `gonzalo-store-fs` (mirrors
  caliban's on-disk layout, the zero-dependency default), `gonzalo-domain`
  (typed `MemoryTier`/`Topic`/`Session`/`Checkpoint` views), and the `gonzalo`
  facade. (ADR 0004, ADR 0008, ADR 0009)
- **Git & S3 substrates + Sync** (M2): `gonzalo-store-git` (commit-per-write,
  fast-forward pull/push) and `gonzalo-store-s3` (S3-compatible object store),
  plus the `Sync` engine reusing the core conflict/merge machinery. (ADR 0004,
  ADR 0005)
- **Daemon + remote substrate** (M3): `gonzalo-proto` (one canonical schema),
  `gonzalo-server` (`gonzalod`) serving the store over **both** gRPC (tonic) and
  HTTP/JSON (axum) on one core service layer with optional bearer auth, and
  `gonzalo-store-server` as the client substrate. (ADR 0007)
- **Vector layer** (M4): `gonzalo-vector` — `Embedder` + `VectorIndex` traits
  with a caller-delegating default embedder and an exact in-memory cosine index.
  (ADR 0008)
- **Code-graph layer** (M5): `gonzalo-graph` — a tree-sitter Rust symbol/ref
  index (`build_rust`) behind a `GraphStore` trait. (ADR 0008)
- **Admin CLI** (M6): `gonzalo-cli` (`gonzalo`) — `list`, `get`, `status`,
  `migrate`, `sync`.

### Internal

- Project: established `docs/adr/` (MADR-lite) with the initial retrospective
  ADRs 0001–0009; added CI (fmt/clippy/build/test), a line-coverage gate, the
  Kanban label taxonomy, and board/triage automation.

[Unreleased]: https://github.com/caliban-ai/gonzalo/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/caliban-ai/gonzalo/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/caliban-ai/gonzalo/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/caliban-ai/gonzalo/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/caliban-ai/gonzalo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/caliban-ai/gonzalo/releases/tag/v0.1.0
