# syntax=docker/dockerfile:1

# ---- builder ----
FROM rust:1.95-bookworm AS builder
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release -p gonzalo-server --bin gonzalod

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN useradd --uid 10001 --create-home --home-dir /home/app --shell /usr/sbin/nologin app
COPY --from=builder /src/target/release/gonzalod /usr/local/bin/gonzalod
# in-cluster defaults: bind all interfaces, store under a mountable /data
ENV GONZALO_ROOT=/data \
    GONZALO_HTTP_ADDR=0.0.0.0:8080 \
    GONZALO_GRPC_ADDR=0.0.0.0:50051
RUN mkdir -p /data && chown -R app:app /data /home/app
USER app
VOLUME ["/data"]
EXPOSE 8080 50051
ENTRYPOINT ["gonzalod"]
