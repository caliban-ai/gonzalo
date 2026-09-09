# Releasing gonzalo to crates.io

gonzalo publishes its facade, library, and binary crates to crates.io from the
**`caliban-ai/gonzalo`** repository only. Publishing is guarded three ways (see
`.github/workflows/publish.yml`): a repo `if`, a `CARGO_REGISTRY_TOKEN` secret
that exists only for this repo, and a tag↔version check. The actual upload runs
through **`scripts/publish.sh`**, which is resumable and rate-limit-aware (see
below).

A `v*` tag drives **three** workflows off the same push, in lockstep:

- `release-image.yml` → builds and pushes the `ghcr.io/caliban-ai/gonzalo`
  container image;
- `publish.yml` → publishes the crate set to crates.io;
- `release-binaries.yml` → builds the Apple Silicon archive and attaches it to
  the GitHub Release (see [Prebuilt binaries](#prebuilt-binaries-macos-apple-silicon)).

One tag, one release — image, crate, and binary versions never drift.

## What gets published

The whole workspace **except** dev/bench harnesses:

- `gonzalo-vector-bench` — excluded from the workspace entirely (`exclude` in the
  root `Cargo.toml`), so `--workspace` never sees it.
- `gonzalo-soak` — a workspace member marked `publish = false` (HA chaos-soak
  harness, gonzalo#52).

Everything else (24 crates) publishes: the `gonzalo` facade, the libraries
(`gonzalo-core`, `gonzalo-store-{fs,git,s3,server}`, `gonzalo-proto`,
`gonzalo-domain`, `gonzalo-vector`, `gonzalo-embed`, `gonzalo-graph`,
`gonzalo-graph-sqlite`, `gonzalo-ticket` + its six provider crates,
`gonzalo-knowledge`, `gonzalo-parse`), and the three binaries
(`gonzalo-server` → `gonzalod`, `gonzalo-cli`, `gonzalo-mcp`). Publishing the
whole workspace (rather than a curated SDK subset) mirrors caliban's approach:
publishing any single crate already requires its full internal-dep closure to be
on the registry, so "publish everything, mark libraries internal/unstable"
avoids subset-closure churn.

Every internal `gonzalo-*` dependency in `[workspace.dependencies]` carries both
a `path` and a `version` — `cargo publish` requires the registry `version` on
every dependency (the `path` is used for the local verify build; the `version`
is what lands in the published manifest).

## The crates.io rate limits — there are two of them

crates.io enforces **two separate limits**, and a 24-crate workspace trips both:

| limit | applies to | when it bites |
|---|---|---|
| **new-crate burst** | publishing crate *names* not seen before | the first release only |
| **updates to existing crates** | every version bump | **every release** |

**New crate names.** A burst of **5**, then **~1 per 10 minutes**
(https://crates.io/docs/rate-limits). The first release of the workspace
therefore cannot go out in one shot — a plain `cargo publish --workspace`
uploads ~5 crates and then fails with HTTP 429. This only bites on the *first*
publish of each name.

**Version bumps of crates that already exist.** These are limited too, by a
separate allowance. Publishing 24 crates back to back exceeds it, so **every**
gonzalo release is expected to stop partway with:

```
error: failed to publish gonzalo-mcp v0.5.0 to registry at https://crates.io
Caused by:
  the remote server responded with an error (status 429 Too Many Requests):
  You have published too many updates to existing crates in a short period of time.
```

That is the documented path, not a broken release — see
[Resuming a rate-limited publish](#resuming-a-rate-limited-publish).

To request a higher limit, email **help@crates.io** with the account and crate
list; they routinely grant it for legitimate multi-crate projects. That is the
only thing that makes a release a single uninterrupted run.

## `scripts/publish.sh`

The publisher handles the limit and partial failures:

- publishes only crates **not yet on crates.io** at the workspace version, so it
  is **idempotent and resumable** — safe to Ctrl-C and re-run;
- publishes in **dependency order** (tsort over intra-workspace path deps), one
  crate at a time, and **skips any `publish = false` crate**;
- uses **`--no-verify`** (CI's `package-check` already verified packaging on the
  release commit, so there is no recompile at publish time);
- on a 429, parses crates.io's "try again after" time and sleeps until then;
- honors **`MAX_SLEEP_SECS`**: locally it sleeps through the windows for free; in
  CI it is set to `0` so the runner never idles (and bills) on a 429.

## One-time setup

1. Create a crates.io API token (scopes: `publish-new` + `publish-update`) and
   add it as the **organization** secret `CARGO_REGISTRY_TOKEN` at
   https://github.com/organizations/caliban-ai/settings/secrets/actions, scoped
   to **selected repositories** (`caliban-ai/gonzalo` and any future publishers)
   — never "all repositories." A repo-level secret of the same name also works
   and takes precedence; pick one home.
2. After the first publish, add the org team as an owner on every crate so
   ownership is shared and the `gonzalo` root name is org-held (this also
   future-proofs RFC 3243 `gonzalo::*` namespacing):

   ```sh
   for c in gonzalo gonzalo-core gonzalo-domain gonzalo-proto \
            gonzalo-store-fs gonzalo-store-git gonzalo-store-s3 gonzalo-store-server \
            gonzalo-vector gonzalo-embed gonzalo-graph gonzalo-graph-sqlite \
            gonzalo-ticket gonzalo-ticket-github gonzalo-ticket-config \
            gonzalo-ticket-jira gonzalo-ticket-linear gonzalo-ticket-gitlab \
            gonzalo-ticket-asana gonzalo-knowledge gonzalo-parse \
            gonzalo-server gonzalo-cli gonzalo-mcp; do
     cargo owner --add github:caliban-ai:<team> "$c"
   done
   ```

## Historical — the 0.3.0 bootstrap

> Completed. Kept for the rate-limit technique, which still applies whenever a
> release introduces a batch of **new** crate names. Nothing here describes the
> current state of the workspace; for a normal release see the next section.

Because of the new-crate rate limit, the initial publish of all 24 crates was run
from a workstation rather than a runner, where the ~10-minute waits cost nothing
(a GitHub runner would bill the idle time). No new tag was needed — the `v0.3.0`
tag already existed, having cut the container image — so the bootstrap was just:

```sh
git checkout main && git pull --ff-only
cargo login                                # a publish-new token
scripts/publish.sh                         # paced, resumable
```

`scripts/publish.sh` skips anything already live and grinds through the rest, so
it can be re-run to resume. **Rotate the token afterward** if it was ever exposed
(e.g. pasted somewhere it could be logged).

## Subsequent releases (version bumps)

These publish new *versions* of existing crates. They **are** rate-limited — by
the update allowance rather than the new-crate one — and 24 crates in a row
exceed it, so expect the workflow to publish most of them and then stop. Cut the
release with **cai-cut-release**
(which bumps the version + internal dep pins in lockstep, rolls the changelog,
and lands the release PR), then:

```sh
git tag vX.Y.Z <merge-sha>
git push origin vX.Y.Z
gh release create vX.Y.Z --title "vX.Y.Z — <theme>" --notes "<the [X.Y.Z] changelog section>"
```

**Create the Release immediately after the tag push**, before waiting on any
workflow. `release-binaries.yml` starts on the same push and has nowhere to
attach its archive until the Release exists; it waits ten minutes and then fails.
Creating the Release needs only the tag, not the crates publish, so there is no
reason to defer it — and doing it here removes the race rather than tolerating it.

The tag push fires all three workflows. `publish.yml` validates the guards and
runs `scripts/publish.sh` with `MAX_SLEEP_SECS=0`, publishing each crate in
dependency order with no recompile.

Throughout, `X.Y.Z` is whatever `[workspace.package].version` in `Cargo.toml`
says after the release PR lands — `publish.yml` refuses a tag that disagrees with
it, so the two cannot drift.

### Resuming a rate-limited publish

**Expect `publish.yml` to go red partway through, and do not read that as a
broken release.** `MAX_SLEEP_SECS=0` in CI means the runner never idles on a 429
— sleeping on a GitHub runner is billed, sleeping on your laptop is free — so
`scripts/publish.sh` publishes what the allowance permits, prints how long the
window has left, and exits 75:

```
⏸ 429: need to wait 615s (> MAX_SLEEP_SECS=0). Stopping; re-run to resume.
==> partial: 23 published, 0 already present this run.
```

Wait out the window it names, then resume. The script skips anything already
live, so re-running is safe and cheap:

```sh
gh run rerun <run-id> --repo caliban-ai/gonzalo --failed
```

`v0.5.0` needed exactly this: 23 of 24 crates published, `gonzalo-mcp` hit the
update limit, and the rerun 615 s later finished in 36 s.

Locally, `scripts/publish.sh` with the default `MAX_SLEEP_SECS` sleeps through
the window on its own instead of exiting.

The same applies, for the other limit, if a release ever introduces more than
~5 **new** crate names: the workflow publishes the burst and stops.

### Confirm every crate is live before creating the Release

A partial publish otherwise ships a GitHub Release pointing at versions that are
not all on crates.io. After `publish.yml` reports success, check the whole set —
and note that the API is not what `cargo` reads, so also see the next section:

```sh
for c in $(cargo metadata --no-deps --format-version 1 \
           | jq -r '.packages[] | select(.publish != []) | .name'); do
  live=$(curl -s "https://crates.io/api/v1/crates/$c/versions" \
         -H 'User-Agent: gonzalo-release (you@example.com)' \
         | jq -r '.versions[0].num')
  [ "$live" = "X.Y.Z" ] || echo "STALE: $c is $live"
done
```

Silence means every crate is at the tagged version. In zsh, capture that crate
list as an array (`crates=(${(f)"$(…)"})`) — zsh does not word-split an
unquoted string, so a plain `for c in $CRATES` iterates once over the whole
blob and reports nothing wrong.

## Prebuilt binaries (macOS, Apple Silicon)

Every release also carries one archive, built by `release-binaries.yml`:

```
gonzalo-vX.Y.Z-aarch64-apple-darwin.tar.gz
gonzalo-vX.Y.Z-aarch64-apple-darwin.tar.gz.sha256
```

It holds `gonzalo`, `gonzalo-mcp`, `gonzalo-parse-worker`, and `LICENSE`.

**Why an archive rather than three `cargo install` lines.** Those three binaries
must agree on `EXTRACTION_VERSION`, and installed separately they drift: a 0.5.0
CLI driving a 0.4.0 worker writes pre-upgrade extraction into a view and then
stamps it current (gonzalo#228), and installing the CLI without the worker
silently drops crash isolation (gonzalo#212). One archive with one of each makes
a mismatch impossible. It also sidesteps the sparse-index lag described in the
next section, since a release asset is immediately consistent.

`gonzalod` is **not** in the archive — the container image is its distribution
channel — and neither is the `gonzalo-soak` harness.

### Installing

```sh
tag=vX.Y.Z
base="https://github.com/caliban-ai/gonzalo/releases/download/$tag"
pkg="gonzalo-$tag-aarch64-apple-darwin"
curl -fsSLO "$base/$pkg.tar.gz" -O "$base/$pkg.tar.gz.sha256"
shasum -a 256 -c "$pkg.tar.gz.sha256"
tar xzf "$pkg.tar.gz"
install -m 755 "$pkg"/gonzalo "$pkg"/gonzalo-mcp "$pkg"/gonzalo-parse-worker ~/.cargo/bin/
```

Keep all three together on `PATH`: `gonzalo index` locates the worker as a
sibling of its own executable, so splitting them re-creates gonzalo#212.
Reconnect any MCP client afterwards — a running server keeps executing the old
binary.

### Gatekeeper

The binaries carry an ad-hoc signature, not a Developer ID one, and are not
notarized. Fetched with `curl` they run as-is, because only quarantine-aware
applications set the attribute. A **browser** download does set it, and macOS
will then refuse to run them. Clear it:

```sh
xattr -d com.apple.quarantine "$pkg"/*
```

### Rehearsing and recovering

`release-binaries.yml` runs on `v*` tags only — no PR build — so it is never
exercised before a real release. The work it does therefore lives in
**`scripts/package-macos.sh`**, which CI merely calls, so the identical chain can
be run first on any Apple Silicon Mac:

```sh
scripts/package-macos.sh          # build, sign, smoke-test, archive into dist/
SKIP_BUILD=1 scripts/package-macos.sh   # repackage without recompiling
```

The script refuses a non-arm64 host and a tag that disagrees with the workspace
version, re-signs after stripping (stripping invalidates the ad-hoc signature and
the binary would otherwise die with `Killed: 9`), and smoke-tests each binary:
`gonzalo --version`, a real parse round-trip through the worker, and an MCP
`initialize` handshake.

The workflow attaches the archive to the **workflow run** before it touches the
Release. So if the Release was created too late and the upload step failed, no
rebuild is needed:

```sh
gh run download <run-id>
gh release upload vX.Y.Z gonzalo-vX.Y.Z-aarch64-apple-darwin.tar.gz{,.sha256} --clobber
```

**Only `aarch64-apple-darwin` is built.** No x86_64 Mac, no Linux (the container
image covers Linux), no Windows. Another target gets added when there is a
concrete request for one, not before.

## Verifying a release — the crates.io API is not what cargo reads

**A crate showing the new version on crates.io does not mean `cargo install` will
get it yet.** The two are different systems:

| surface | consistency |
|---|---|
| `/api/v1/crates/<name>` and the web UI | reads the database — immediate |
| the **sparse index** cargo resolves against | CDN-cached — can lag ~an hour |

So the obvious post-release check ("crates.io lists X.Y.Z") does not verify the
thing that actually matters. After `v0.4.0`, an install run about an hour after
publishing silently produced `gonzalo-cli` at 0.4.0 but `gonzalo-mcp` and
`gonzalo-parse` still at 0.3.0 — no error, no warning, and only
`cargo install --list` showed it. Every 0.4.0 crate was published and unyanked at
the time; it was purely index freshness.

Pin the version when installing, so a stale index fails loudly instead of quietly
serving the old build:

```sh
cargo install gonzalo-cli@X.Y.Z gonzalo-mcp@X.Y.Z gonzalo-parse@X.Y.Z
```

If that errors with no matching package, the index has not caught up — wait and
retry. Then confirm what actually landed, and force anything stale:

```sh
cargo install --list | grep -E 'gonzalo-(cli|mcp|parse)'
cargo install --force gonzalo-mcp@X.Y.Z        # if it came back at the old version
```

Note that `--force` is also what you need after a *code* change at the same
version, and that an MCP client must be reconnected to pick up a newly installed
binary — a running server keeps executing the old one.

## If a publish fails partway

For the routine case — a 429 on the update limit, which is most of them — see
[Resuming a rate-limited publish](#resuming-a-rate-limited-publish).

For any other partial failure the recovery is the same shape. crates.io releases
are immutable, so already-published crates cannot be re-uploaded at the same
version. Re-run **`scripts/publish.sh`**: it skips everything already live and
continues with the rest. (By hand, if you must: `cargo publish -p <crate>
--no-verify` for each remaining crate, in dependency order.)
