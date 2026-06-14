# ADR 0007 · Dual-transport daemon: gRPC + HTTP/JSON over one schema

- **Status:** accepted
- **Date:** 2026-06-13

## Context

The optional daemon (`gonzalod`) lets non-Rust tools and remote systems share a
Gonzalo store. Two audiences pull in opposite directions: Rust clients and
streaming large transfers want strongly-typed **gRPC**; ad-hoc tooling and
humans want a **curl-able HTTP/JSON** API. Maintaining two independent
implementations of the same surface would let them drift apart.

## Decision

`gonzalo-server` exposes **both** transports — a `tonic` gRPC service and an
`axum` HTTP/JSON service — over **one shared core service layer**.
`gonzalo-proto` holds the single canonical schema both derive from: protobuf for
gRPC, serde types for HTTP/JSON. Payloads are JSON-encoded `gonzalo-core`
records carried as bytes, so both transports share one serialization and stay in
lockstep. The daemon supports optional bearer-token auth with namespace-scoped
checks. `gonzalo-store-server` is the client side, speaking either transport.

## Consequences

- **Positive:** Operators pick the transport that fits — Rust clients get typed
  gRPC, everyone else gets curl. One service layer and one schema mean the two
  transports cannot drift. Auth is one concern handled at one layer.
- **Negative:** Two server stacks (tonic + axum) to build and keep running.
  Carrying JSON-over-bytes inside protobuf forgoes some of gRPC's native typed
  payloads for the sake of a single shared serialization.
- **Revisit if:** one transport goes effectively unused (drop it), or
  performance demands native protobuf payloads instead of JSON-over-bytes.
