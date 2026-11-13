# syntax=docker/dockerfile:1.7
#
# The image of the `ptc` binary, as Railway builds it (deploy/railway/README.md): the server
# stage of the repository's Dockerfile, without its cache mounts, whose ids Railway restricts to
# one service. The api and the worker build it each, and choose their role by their start
# command (`/usr/local/bin/ptc server --role api`).

ARG RUST_VERSION=1.99
ARG NODE_VERSION=22
ARG DEBIAN=trixie

# The dashboard's production build, embedded by ptc-server's build script.
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
ENV PTC_DASHBOARD_DIST=/dashboard-dist TZ=UTC
RUN cargo build --release --locked -p ptc \
    && install -D -m 0755 target/release/ptc /out/ptc \
    && install -d /out/cache \
    && rm -rf target

FROM gcr.io/distroless/cc-debian13:nonroot
LABEL org.opencontainers.image.title="ptc" \
      org.opencontainers.image.description="ai-ptc on Railway: the api or the worker, chosen by the start command"
COPY --from=build /out/ptc /usr/local/bin/ptc
COPY --from=build --chown=65532:65532 /out/cache /var/cache/ptc
# Railway's private network is IPv6: listen on every address, IPv4 and IPv6.
ENV TZ=UTC \
    PTC_LISTEN=[::]:8080 \
    PTC_TSGO_CACHE=/var/cache/ptc/tsgo
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/ptc"]
CMD ["server", "--help"]
