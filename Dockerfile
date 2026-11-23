# syntax=docker/dockerfile:1.7
#
# The image of the `rewind` binary: one image for every node, chosen by its arguments
# (`rewind server`, `rewind server --executors-only`). The dashboard is built with pnpm and
# embedded in the binary; tsgo, the type-checker, is embedded by rewind-check's build script.
#
#   docker build -t rewind:local .                     # the server image (the last stage)
#   docker build -t rewind-load:local --target load .  # the load generator of deploy/
#
# The runtime is distroless (glibc, CA certificates, no shell) and runs as the user `nonroot`
# (65532). The binary needs TZ=UTC, which the image sets.

ARG RUST_VERSION=1.99
ARG NODE_VERSION=22
ARG DEBIAN=trixie

# The dashboard's production build, embedded by rewind-server's build script.
FROM node:${NODE_VERSION}-${DEBIAN}-slim AS dashboard
ENV PNPM_HOME=/pnpm COREPACK_ENABLE_DOWNLOAD_PROMPT=0
RUN corepack enable
WORKDIR /src/dashboard
COPY dashboard/package.json dashboard/pnpm-lock.yaml ./
RUN --mount=type=cache,id=rewind-pnpm-store,target=/pnpm/store \
    pnpm install --frozen-lockfile
COPY dashboard/ ./
RUN pnpm build

# The workspace's sources, and the tool chain.
FROM rust:${RUST_VERSION}-${DEBIAN} AS sources
WORKDIR /src
COPY . .
COPY --from=dashboard /src/dashboard/dist /dashboard-dist
ENV REWIND_DASHBOARD_DIST=/dashboard-dist TZ=UTC
# Parallel jobs of cargo; all the CPUs when empty.
ARG JOBS=

FROM sources AS build
RUN --mount=type=cache,id=rewind-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rewind-target-release,target=/src/target,sharing=locked \
    cargo build --release --locked ${JOBS:+--jobs $JOBS} -p rewind \
    && install -D -m 0755 target/release/rewind /out/rewind \
    && install -d /out/cache

FROM sources AS build-load
RUN --mount=type=cache,id=rewind-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rewind-target-release,target=/src/target,sharing=locked \
    cargo build --release --locked ${JOBS:+--jobs $JOBS} -p rewind-load \
    && install -D -m 0755 target/release/rewind-load /out/rewind-load

# The load generator and fake MCP server of the reference deployment (deploy/README.md).
FROM gcr.io/distroless/cc-debian13:nonroot AS load
COPY --from=build-load /out/rewind-load /usr/local/bin/rewind-load
USER nonroot
ENTRYPOINT ["/usr/local/bin/rewind-load"]
CMD ["--help"]

# The server and CLI.
FROM gcr.io/distroless/cc-debian13:nonroot AS rewind
LABEL org.opencontainers.image.title="rewind" \
      org.opencontainers.image.description="Rewind: a node of the run plane, its HTTP API and the dashboard, as its arguments choose"
COPY --from=build /out/rewind /usr/local/bin/rewind
COPY --from=build --chown=65532:65532 /out/cache /var/cache/rewind
# tsgo is extracted here at the first type-check, by the node that checks: give it a writable
# directory that allows execution when the root file system is read-only.
ENV TZ=UTC \
    REWIND_LISTEN=0.0.0.0:8080 \
    REWIND_TSGO_CACHE=/var/cache/rewind/tsgo
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/rewind"]
CMD ["server", "--help"]
