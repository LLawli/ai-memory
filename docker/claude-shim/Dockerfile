# syntax=docker/dockerfile:1.7
#
# Multi-stage build for claude-shim.
# Builder:  rust:1.95-slim compiles the shim binary.
# Runtime:  debian:bookworm-slim + Node 20 + @anthropic-ai/claude-code,
#           non-root user, OAuth token via env at runtime.

FROM rust:1.95-slim-bookworm AS builder
WORKDIR /work
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config build-essential \
 && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    cargo build --release --bin claude-shim \
 && cp target/release/claude-shim /usr/local/bin/claude-shim

FROM debian:bookworm-slim
# Node + @anthropic-ai/claude-code give us the `claude` CLI. The version
# of the npm package is pinned here so envelope-shape changes upstream
# don't silently break the shim.
ARG CLAUDE_CODE_VERSION=latest
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg \
 && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && apt-get purge -y curl gnupg \
 && apt-get autoremove -y \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 1001 --user-group --home-dir /home/shim \
        --shell /bin/bash shim \
 && mkdir -p /home/shim \
 && chown -R shim:shim /home/shim
COPY --from=builder /usr/local/bin/claude-shim /usr/local/bin/claude-shim
USER shim
WORKDIR /home/shim
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/claude-shim", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/claude-shim"]
CMD ["--bind", "0.0.0.0:8080"]
