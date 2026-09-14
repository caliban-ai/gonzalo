# Running gonzalod

`gonzalod` serves one store over two transports at once: HTTP/JSON (axum) and gRPC
(tonic), both generated from one schema in `gonzalo-proto`
([ADR 0007](./adr/0007-dual-transport-daemon.md)). It is the integration point for
anything that is not a Rust program linking the `gonzalo` crate
([ADR 0020](./adr/0020-rust-native-deliverables.md)).

## Run it

From a checkout:

```sh
cargo run --release -p gonzalo-server --bin gonzalod
```

Or the container image, which binds all interfaces and keeps data in a `/data`
volume:

```sh
docker run --rm -p 8080:8080 -p 50051:50051 \
  -v gonzalo-data:/data ghcr.io/caliban-ai/gonzalo
```

On startup it prints one line naming what it chose:

```
gonzalod: store fs(./gonzalo-data), HTTP on 127.0.0.1:8080, gRPC on 127.0.0.1:50051, auth off
```

## Configuration

All configuration is environment variables. A malformed value is a startup error, not
a silent fallback.

| variable | purpose | default |
|---|---|---|
| `GONZALO_STORE` | substrate: `fs` or `s3` | `fs` |
| `GONZALO_ROOT` | `fs` store root | `./gonzalo-data` (image: `/data`) |
| `GONZALO_S3_BUCKET` | bucket, required when `GONZALO_STORE=s3` | none |
| `GONZALO_S3_ENDPOINT` | S3 endpoint for a self-hosted store such as RustFS | ambient AWS config |
| `GONZALO_S3_REGION` | region override | ambient AWS config |
| `AWS_*` | S3 credentials, read the standard way | none |
| `GONZALO_HTTP_ADDR` | HTTP bind address | `127.0.0.1:8080` (image: `0.0.0.0:8080`) |
| `GONZALO_GRPC_ADDR` | gRPC bind address | `127.0.0.1:50051` (image: `0.0.0.0:50051`) |
| `GONZALO_MAX_BLOB_SIZE` | max bytes per blob over the transports | 64 MiB |
| `GONZALO_AUTH_FILE` | TOML principals file for namespace-scoped auth | unset |
| `GONZALO_TOKEN` | single admin token, used only when no auth file is set | unset |

`GONZALO_MAX_BLOB_SIZE` also raises the gRPC decode limit for every RPC, because
tonic's limit is per server rather than per method. HTTP scopes it to the blob routes
only ([#194](https://github.com/caliban-ai/gonzalo/issues/194)).

With `fs`, code-graph views written by `gonzalo index` under `<root>/graphs` are
queryable through the daemon. With `s3` there is no local graph cache; views assemble
from their manifest and content-addressed slices on demand.

Git is not a daemon substrate. See [Storage backends](./storage.md) for choosing
between `fs` and `s3`, and for which S3 implementations are safe to run several
replicas against.

## Auth

Auth is resolved in this order ([ADR 0015](./adr/0015-namespace-scoped-daemon-auth.md)):

1. `GONZALO_AUTH_FILE` set: parse it as a principals file.
2. Otherwise `GONZALO_TOKEN` set: one admin principal named `root` with that token.
3. Otherwise auth is **off**. Every request is served as an implicit admin.

A principals file maps bearer tokens to named principals with per-namespace scopes.
`"*"` means every namespace:

```toml
[[principal]]
name  = "caliban"
token = "s3cret"
read  = ["memory", "sessions"]
write = ["memory"]

[[principal]]
name  = "admin"
token = "root"
read  = ["*"]
write = ["*"]
```

Duplicate tokens are rejected at startup. Clients send `Authorization: Bearer <token>`
as an HTTP header or as gRPC `authorization` metadata. A missing or unknown token is
`401`. A known token without the needed scope is `403`.

What each operation needs:

| operation | needs |
|---|---|
| get a record | `read` on its namespace |
| put or delete a record | `write` on its namespace |
| list keys in a namespace | `read` on that namespace |
| list keys with no namespace filter | `read` on `"*"` |
| raw get / raw put a record | `read` / `write` on its namespace |
| raw key listing | as list keys |
| purge a record | admin (`"*"` in both lists) |
| code-graph queries | `read` on the view's `repo` |
| blobs | `read` / `write` on the reserved `_blobs` namespace |
| ticket sync | `write` on `tickets` |
| `/healthz`, `/readyz` | nothing: probes bypass auth |

When auth is on, a write's `meta.author` is overwritten with the authenticated
principal's name, so authorship cannot be forged — except `put_raw` and `delete`
from an admin, which keep the replicated author or the named deleter. With auth
off, the author the client sent is kept. See
[Deletion, reset & collection § Over the daemon](./deletion.md#over-the-daemon)
for the full authorship rules.

## HTTP API

| method and path | does |
|---|---|
| `GET /healthz` | liveness: always `200 ok` |
| `GET /readyz` | readiness: `200` when the backing store is reachable, else `503` |
| `GET /v1/records/{ns}/{col}/{id}` | the record as JSON, or `404` |
| `PUT /v1/records/{ns}/{col}/{id}` | body `{"record": …, "expected": <revision or null>}` |
| `DELETE /v1/records/{ns}/{col}/{id}` | optional body `{"expected": <revision>, "author": <identity>}`, both fields optional |
| `GET /v1/keys?namespace=&collection=` | list keys, both filters optional |
| `GET /v1/raw/records/{ns}/{col}/{id}` | the record as JSON including tombstones; always `200`, absence is `{"record": null}` |
| `PUT /v1/raw/records/{ns}/{col}/{id}` | replication write; body `{"record": …, "expected": <revision or null>}`, stored verbatim |
| `GET /v1/raw/keys?namespace=&collection=` | list keys including tombstoned ones, both filters optional |
| `POST /v1/purge/{ns}/{col}/{id}` | body `{"expected": <revision>}`; physically removes the record |
| `GET`, `PUT`, `DELETE /v1/blobs/{hash}` | content-addressed blob bytes |
| `GET /v1/blobs` | list blob hashes |
| `POST /v1/tickets/sync` | sync one ticket connection; the body is one `[[connection]]` entry from `tickets.toml`, as JSON |
| `GET /v1/graph/{definitions,references,callers,callees,impact}?repo=&view=&name=` | code-graph queries against an indexed view |

A put whose URL disagrees with the record's key is `400`. A put or delete whose
`expected` revision is stale returns `409` with `{"outcome":"conflict","conflict":…}`
carrying the live record. A successful put returns
`{"outcome":"committed","revision":…}`. Conflicts are normal results, not errors
([ADR 0005](./adr/0005-optimistic-concurrency-and-conflict-surfacing.md)).

A put rejected as not found — a conditional write (normal or raw) whose expected
revision the store no longer holds, the common case being a write over a deleted
key — returns `412`. Other internal failures return an opaque `500`; the detail
goes to the daemon's stderr.

## gRPC API

The `gonzalo` service in `gonzalo-proto` carries the same operations: `Get`, `Put`,
`Delete`, `List`, `GetRaw`, `ListRaw`, `PutRaw`, `Purge`, `PutBlob`, `GetBlob`,
`ListBlobs`, `DeleteBlob`, `TicketSync`, and `GraphDefinitions`, `GraphReferencesTo`,
`GraphCallersOf`, `GraphCallees`, `GraphImpact`. Authorization is identical to
HTTP.

## From Rust

A Rust program can use a daemon as an ordinary `Store`. Enable the facade's `remote`
feature and construct a `ServerStore` with `ServerStore::http(url)` or
`ServerStore::grpc(endpoint)`, or their `*_with_token` variants. Any code written
against `Store` then runs unchanged against the daemon.

## Container notes

The image is `ghcr.io/caliban-ai/gonzalo`, built from the repository's `Dockerfile`.
It runs as uid 10001 and exposes `8080` and `50051`. Point liveness at `/healthz` and
readiness at `/readyz`; neither needs a token. See also `docs/container.md` in the
repository.
