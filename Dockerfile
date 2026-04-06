# Stage 1 — builder
# Compile the workspace in release mode. Cargo's build cache is not preserved
# between Docker builds by default; add --mount=type=cache,target=/build/target
# if you are using BuildKit and want layer caching.
FROM rust:1.88-bookworm AS builder

WORKDIR /build

# Copy the workspace manifest and lockfile first so the dependency-download
# layer is only invalidated when dependencies change, not when source changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN cargo build --release --bin raft-server

# Stage 2 — runtime
# Use a minimal Debian image. The binary links against glibc so we cannot use
# scratch here.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/raft-server /usr/local/bin/raft-server

# Data directory for durable state. Mount a named volume here in production.
RUN mkdir /data

EXPOSE 7001
EXPOSE 9001

ENTRYPOINT ["/usr/local/bin/raft-server"]
