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
| `GONZALO_AUTH_FILE` | TOML principals file for namespace-scoped auth (ADR 0015) | unset |
| `GONZALO_TOKEN` | single admin token, used when no auth file is set | unset |

With neither `GONZALO_AUTH_FILE` nor `GONZALO_TOKEN` set, auth is off.

For more than one replica over S3, the backend must enforce `If-Match`
conditional writes atomically. RustFS is the qualified backend; Garage is not
safe (ADR 0019).

## Probes

`GET /healthz` (liveness, always `200`) and `GET /readyz` (`200` when the backing
store is reachable, `503` otherwise) are served on the HTTP port and need no
token.
