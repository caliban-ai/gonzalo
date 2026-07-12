# Releasing gonzalo to crates.io

gonzalo publishes its facade, library, and binary crates to crates.io from the
**`caliban-ai/gonzalo`** repository only. Publishing is guarded three ways (see
`.github/workflows/publish.yml`): a repo `if`, a `CARGO_REGISTRY_TOKEN` secret
that exists only for this repo, and a tag↔version check. The actual upload runs
through **`scripts/publish.sh`**, which is resumable and rate-limit-aware (see
below).

A `v*` tag drives **two** workflows off the same push, in lockstep:

- `release-image.yml` → builds and pushes the `ghcr.io/caliban-ai/gonzalo`
  container image;
- `publish.yml` → publishes the crate set to crates.io.

One tag, one release — the image and crate versions never drift.

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

## The crates.io new-crate rate limit

crates.io throttles the creation of **brand-new crate names** much harder than
new *versions* of existing crates: a burst of **5 new crates**, then **~1 new
crate per 10 minutes** (https://crates.io/docs/rate-limits). The first release
of the workspace therefore cannot go out in one shot — a plain
`cargo publish --workspace` uploads ~5 crates and then fails with HTTP 429.

This only bites on the **first** publish of each crate name. Once all 24 crates
exist, future releases publish new *versions*, which are not meaningfully
limited.

To request a higher limit, email **help@crates.io** with the account and crate
list; they routinely grant it for legitimate multi-crate projects.

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

## First publish (many new crates) — run locally

Because of the rate limit, the very first publish is best run from your machine,
where the ~10-minute waits cost nothing (a GitHub runner would bill the idle
time). The workspace is already at `0.3.0` and the `v0.3.0` tag already exists
(it cut the container image), so **no new tag is needed** — just authenticate
and run the resumable publisher against the current `0.3.0` checkout of `main`:

```sh
git checkout main && git pull --ff-only    # be on the 0.3.0 commit
cargo login                                # paste a publish-new token
scripts/publish.sh                         # 24 crates → paced, resumable
```

It skips anything already live and grinds through the rest. Re-run it any time
to resume. **Rotate the token afterward** if it was ever exposed (e.g. pasted
where it could be logged).

The client crates caliban#469 needs — `gonzalo-core`, `gonzalo-store-server`,
`gonzalo-store-fs` and their transitive closure (`gonzalo-proto`,
`gonzalo-domain`) — publish early in dependency order, so caliban is unblocked
well before the full workspace finishes.

## Subsequent releases (version bumps)

These publish new *versions* of existing crates and are not rate-limited, so the
workflow handles them automatically. Cut the release with **cai-cut-release**
(which bumps the version + internal dep pins in lockstep, rolls the changelog,
and lands the release PR), then:

```sh
git tag vX.Y.Z <merge-sha>
git push origin vX.Y.Z
```

The tag push fires both `release-image.yml` (image) and `publish.yml` (crates).
`publish.yml` validates the guards and runs `scripts/publish.sh` with
`MAX_SLEEP_SECS=0`, publishing each crate in dependency order with no recompile.

If a release ever introduces **new** crate names and there are more than ~5 of
them, the workflow publishes the burst and stops (it won't idle-bill on the
429) — finish the rest locally with `scripts/publish.sh`.

## If a publish fails partway

crates.io releases are immutable, so already-published crates cannot be
re-uploaded at the same version. Recovery is simply to **re-run
`scripts/publish.sh`** — it skips everything already live and continues with the
rest. (If you must do it by hand: `cargo publish -p <crate> --no-verify` for each
remaining crate, in dependency order.)
