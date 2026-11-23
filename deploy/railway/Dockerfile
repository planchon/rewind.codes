# syntax=docker/dockerfile:1.7
#
# The image of the `rewind` binary, as Railway builds it (deploy/railway/README.md): the server
# stage of the repository's Dockerfile, without its cache mounts, whose ids Railway restricts to
# one service. The node service builds it, and starts it with its start command
# (`/usr/local/bin/rewind server`).

ARG RUST_VERSION=1.99
ARG NODE_VERSION=22
ARG DEBIAN=trixie

# The dashboard's production build, embedded by rewind-server's build script.
FROM node:${NODE_VERSION}-${DEBIAN}-slim AS dashboard
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0
RUN corepack enable
WORKDIR /src/dashboard
COPY dashboard/package.json dashboard/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY dashboard/ ./
RUN pnpm build

FROM rust:${RUST_VERSION}-${DEBIAN} AS build
WORKDIR /src
COPY . .
COPY --from=dashboard /src/dashboard/dist /dashboard-dist
ENV REWIND_DASHBOARD_DIST=/dashboard-dist TZ=UTC
RUN cargo build --release --locked -p rewind \
    && install -D -m 0755 target/release/rewind /out/rewind \
    && install -d /out/cache \
    && rm -rf target

FROM gcr.io/distroless/cc-debian13:nonroot
LABEL org.opencontainers.image.title="rewind" \
      org.opencontainers.image.description="Rewind on Railway: rewind server, an HTTP edge and a node of the run plane"
COPY --from=build /out/rewind /usr/local/bin/rewind
COPY --from=build --chown=65532:65532 /out/cache /var/cache/rewind
# Railway's private network is IPv6: the edge and the node link listen on every address, IPv4
# and IPv6.
ENV TZ=UTC \
    REWIND_LISTEN=[::]:8080 \
    REWIND_LINK_LISTEN=[::]:7070 \
    REWIND_TSGO_CACHE=/var/cache/rewind/tsgo
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/rewind"]
CMD ["server", "--help"]
