# Container image

`ghcr.io/caliban-ai/gonzalo` runs the `gonzalod` persistence daemon (HTTP +
gRPC). The image binds `0.0.0.0` and stores data under `/data` (a mountable
volume). It uses the **filesystem substrate** — the daemon does not select S3
at runtime today (see the k8s design spec's gonzalo HA prerequisites).

## Run

    docker run --rm -p 8080:8080 -p 50051:50051 \
      -v gonzalo-data:/data ghcr.io/caliban-ai/gonzalo

## Environment

| Var | Purpose | Image default |
|-----|---------|---------------|
| `GONZALO_ROOT` | fs store root | `/data` |
| `GONZALO_HTTP_ADDR` | HTTP bind | `0.0.0.0:8080` |
| `GONZALO_GRPC_ADDR` | gRPC bind | `0.0.0.0:50051` |
| `GONZALO_TOKEN` | require `Authorization: Bearer <token>` | unset (auth off) |

There is no HTTP health endpoint; liveness/readiness use a TCP check on `8080`.
