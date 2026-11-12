# syntax=docker/dockerfile:1.7
#
# The image of the `ptc` binary: one image for every role, chosen by its arguments
# (`ptc server --role api`, `ptc server --role worker`). The dashboard is built with pnpm and
# embedded in the binary; tsgo, the type-checker, is embedded by ptc-check's build script.
#
#   docker build -t ptc:local .                     # the server image (the last stage)
#   docker build -t ptc-load:local --target load .  # the load generator of deploy/
#
# The runtime is distroless (glibc, CA certificates, no shell) and runs as the user `nonroot`
# (65532). The binary needs TZ=UTC, which the image sets.

ARG RUST_VERSION=1.99
ARG NODE_VERSION=22
ARG DEBIAN=trixie

# The dashboard's production build, embedded by ptc-server's build script.
FROM node:${NODE_VERSION}-${DEBIAN}-slim AS dashboard
ENV PNPM_HOME=/pnpm COREPACK_ENABLE_DOWNLOAD_PROMPT=0
RUN corepack enable
WORKDIR /src/dashboard
COPY dashboard/package.json dashboard/pnpm-lock.yaml ./
RUN --mount=type=cache,id=ptc-pnpm-store,target=/pnpm/store \
    pnpm install --frozen-lockfile
COPY dashboard/ ./
RUN pnpm build

# The workspace's sources, and the tool chain.
FROM rust:${RUST_VERSION}-${DEBIAN} AS sources
WORKDIR /src
COPY . .
COPY --from=dashboard /src/dashboard/dist /dashboard-dist
ENV PTC_DASHBOARD_DIST=/dashboard-dist TZ=UTC
# Parallel jobs of cargo; all the CPUs when empty.
ARG JOBS=

FROM sources AS build
RUN --mount=type=cache,id=ptc-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=ptc-target-release,target=/src/target,sharing=locked \
    cargo build --release --locked ${JOBS:+--jobs $JOBS} -p ptc \
    && install -D -m 0755 target/release/ptc /out/ptc \
    && install -d /out/cache

FROM sources AS build-load
RUN --mount=type=cache,id=ptc-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=ptc-target-release,target=/src/target,sharing=locked \
    cargo build --release --locked ${JOBS:+--jobs $JOBS} -p ptc-load \
    && install -D -m 0755 target/release/ptc-load /out/ptc-load

# The load generator and fake MCP server of the reference deployment (deploy/README.md).
FROM gcr.io/distroless/cc-debian13:nonroot AS load
COPY --from=build-load /out/ptc-load /usr/local/bin/ptc-load
USER nonroot
ENTRYPOINT ["/usr/local/bin/ptc-load"]
CMD ["--help"]

# The server and CLI.
FROM gcr.io/distroless/cc-debian13:nonroot AS ptc
LABEL org.opencontainers.image.title="ptc" \
      org.opencontainers.image.description="ai-ptc: the HTTP API, the workers and the dashboard; the role is chosen by the arguments"
COPY --from=build /out/ptc /usr/local/bin/ptc
COPY --from=build --chown=65532:65532 /out/cache /var/cache/ptc
# tsgo is extracted here at the first type-check, by the api role: give it a writable
# directory that allows execution when the root file system is read-only.
ENV TZ=UTC \
    PTC_LISTEN=0.0.0.0:8080 \
    PTC_TSGO_CACHE=/var/cache/ptc/tsgo
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/ptc"]
CMD ["server", "--help"]
