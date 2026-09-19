# Container image

`ghcr.io/caliban-ai/gonzalo` runs the `gonzalod` persistence daemon (HTTP +
gRPC). The image binds `0.0.0.0`, runs as uid 10001, and by default stores data
under `/data` (a mountable volume) with the **filesystem substrate**. Set
`GONZALO_STORE=s3` to back it with an S3-compatible object store instead.

The guide's [Running gonzalod](https://caliban-ai.github.io/gonzalo/daemon.html)
page is the full reference for configuration, auth and the API.

## Run

    docker run --rm -p 8080:8080 -p 50051:50051 \
      -v gonzalo-data:/data ghcr.io/caliban-ai/gonzalo

## Environment

| Var | Purpose | Image default |
|-----|---------|---------------|
| `GONZALO_STORE` | substrate: `fs` or `s3` | `fs` |
| `GONZALO_ROOT` | fs store root | `/data` |
| `GONZALO_S3_BUCKET` | S3 bucket (required when `GONZALO_STORE=s3`) | unset |
| `GONZALO_S3_ENDPOINT` | S3 endpoint for a self-hosted store (e.g. RustFS) | unset (ambient AWS config) |
| `GONZALO_S3_REGION` | S3 region override | unset (ambient AWS config) |
| `AWS_*` | S3 credentials | unset |
| `GONZALO_HTTP_ADDR` | HTTP bind | `0.0.0.0:8080` |
| `GONZALO_GRPC_ADDR` | gRPC bind | `0.0.0.0:50051` |
| `GONZALO_MAX_BLOB_SIZE` | max bytes per blob (also the gRPC decode ceiling) | 64 MiB |
| `GONZALO_ANCESTOR_CAP` | most recent revisions each record remembers, at least 1 (ADR 0021) | 32 |
| `GONZALO_AUTH_FILE` | TOML principals file for namespace-scoped auth (ADR 0015) | unset |
| `GONZALO_TOKEN` | single admin token, used when no auth file is set | unset |

## Auth

`gonzalod` picks its auth mode at startup. The first that applies wins:

1. **`GONZALO_AUTH_FILE`** — a principals file, scoped per namespace (below).
2. **`GONZALO_TOKEN`** — a single admin principal, named `root`, holding that
   token.
3. **Neither** — auth is off, and every request is served as an implicit admin.

An empty value counts as unset. If `GONZALO_AUTH_FILE` names a file that is
missing or doesn't parse, or two principals share a token, `gonzalod` refuses to
start. It never falls back to running open.

### The principals file

One `[[principal]]` entry per client. The bearer token a request sends picks
the principal:

```toml
# A client scoped to the namespaces it owns.
[[principal]]
name  = "ariel"                     # stamped as the author of this client's writes
token = "<a long random secret>"    # the bearer token the client sends
read  = ["fleet", "fleet-audit"]    # namespaces it may read
write = ["fleet", "fleet-audit"]    # namespaces it may write

# An admin: "*" matches every namespace.
[[principal]]
name  = "admin"
token = "<another long random secret>"
read  = ["*"]
write = ["*"]
```

- `read` and `write` list namespace names, matched exactly; `"*"` matches any.
  Either list may be left out, which grants no access of that kind.
- **Blobs** belong to no namespace. They authorize against the reserved
  `_blobs` namespace, so a scoped client that stores blob-backed records needs
  `_blobs` in its lists.
- A principal with `"*"` in **both** lists is an admin. Listing keys without
  naming a namespace needs `read` on `"*"`; purging tombstones needs an admin.

The file holds tokens in plain text. Mount it read-only from a secret:

    docker run --rm -p 8080:8080 -p 50051:50051 \
      -v gonzalo-data:/data \
      -v "$PWD/principals.toml:/etc/gonzalo/principals.toml:ro" \
      -e GONZALO_AUTH_FILE=/etc/gonzalo/principals.toml \
      ghcr.io/caliban-ai/gonzalo

Clients send `Authorization: Bearer <token>` over HTTP, and the same value in
the `authorization` metadata over gRPC.

### When a request is refused

| Situation | HTTP | gRPC |
|---|---|---|
| No token, or one no principal holds | `401` | `UNAUTHENTICATED` |
| Valid token, namespace not in its `read`/`write` list | `403` | `PERMISSION_DENIED` |
| An admin-only operation from a non-admin | `403` | `PERMISSION_DENIED` |

The [probes](#probes) need no token, so a liveness or readiness check keeps
working with auth on.

### Authorship

With auth on, `gonzalod` stamps each write's author with the principal's `name`,
whatever the client sent, so a client cannot write under someone else's name.
Two exceptions apply to admins only: a delete may name another principal as the
deleter, and a replication write keeps the author of the record it copies. That
is what lets an admin token carry other writers' records between stores. A
non-admin is always stamped as itself.

With auth off there is no identity to stamp, and a record keeps the author the
client wrote.

For more than one replica over S3, the backend must enforce `If-Match`
conditional writes atomically. RustFS is the qualified backend; Garage is not
safe (ADR 0019).

## Probes

`GET /healthz` (liveness, always `200`) and `GET /readyz` (`200` when the backing
store is reachable, `503` otherwise) are served on the HTTP port and need no
token.
