# ADR 0009 · Workspace layout and single-facade public surface

- **Status:** accepted
- **Date:** 2026-06-13

## Context

Gonzalo is 12 crates (core; the fs/git/s3/server substrates; proto; server;
domain; vector; graph; cli; and the facade). Caliban should not have to depend
on, and feature-wrangle, all of them. We also want heavy dependencies (git2,
aws-sdk, tonic) to be opt-in so a default consumer stays lean.

## Decision

Use a single Cargo **workspace** (edition 2024, rust-version 1.95), with every
crate prefixed `gonzalo-` except the facade. The **`gonzalo` facade** is a thin
re-export crate giving caliban one dependency and a curated public surface;
substrates and capability layers are selected via the facade's Cargo features
(`fs`, `git`, `s3`, `remote`, `vector`, `graph`). Internal crate versions are
centralized in `[workspace.dependencies]`, and `unsafe_code` is forbidden
workspace-wide. The two binaries are `gonzalod` (daemon) and `gonzalo` (CLI).

## Consequences

- **Positive:** Caliban depends on one crate and turns capabilities on by
  feature; default builds avoid the heavy deps. Single-responsibility crates
  keep compile units and ownership clear. Centralized workspace deps and lints
  keep the tree consistent.
- **Negative:** Twelve crates is real overhead — a change touching the core can
  ripple through the workspace, and the facade must be kept in sync with what it
  re-exports. Feature combinations need testing to ensure each builds.
- **Revisit if:** the crate count becomes a maintenance burden out of proportion
  to the isolation it buys, or feature combinations prove untestable.
