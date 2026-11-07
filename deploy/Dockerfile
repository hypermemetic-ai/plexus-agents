# syntax=docker/dockerfile:1
# plexus-agents' image (SUPERCONDUCTOR-20): the `agents` service alone on a
# slim runtime, for hypermemetic-infra's box behind its edge (deploy/app.yml
# beside this file) — agents-test.menger.sh, and agents.menger.sh later.
#
# plexus-agents takes plexus-harness by path (unpublished), so the build
# context is not a checkout but one assembled by build-image.sh: plexus-agents
# at the commit being built, and each sibling at the commit pinned here
# (`git archive`, nothing of a working tree):
#
# pin plexus-harness 7f42216f8aa1868fc2969563d72b9fe9e36855a4
# pin plexus-login b48bdac663d5928d9891bd3bdc287cc07a8222ae
#
# (plexus-login by path too, for SUPERCONDUCTOR-25's allow list, until it is
# published: Cargo.toml's [patch.crates-io].)
#
#   deploy/build-image.sh [COMMIT]   → ghcr.io/hypermemetic-ai/agents:<commit>
#
# The hosted service runs no Claude: there is no `claude` binary in this
# image, and deploy/env/<env>.env sets AGENTS_NO_LOCAL=1 — runs go to hosts
# that dial in (agents.on.<host>).
#
# One port, 4470, plain, bound to 0.0.0.0 inside the container and published
# nowhere: plexus-ui's shell (/, /app/…), the sign-in routes (/login…) and the
# socket (/ws, or an upgrade anywhere) all on it (SUPERCONDUCTOR-11). The page is
# not in this image: it is a plexus-ui bundle, pushed (deploy/push-ui.sh) into
# /data/ui.db and kept there across deploys.
# The edge terminates TLS and reaches it over its `edge` network.
# plexus-agents reads every flag from AGENTS_* (deploy/env/<env>.env, the box's
# secret files, the box's AGENTS_TRUSTED_PROXIES).

FROM rust:1.97-bookworm AS build
WORKDIR /src
COPY . .
# The caches are BuildKit's, not the image's: a rebuild recompiles only what
# changed.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/plexus-agents/target \
    cd plexus-agents \
 && cargo build --release --locked --bin plexus-agents \
 && install -m 0755 target/release/plexus-agents /usr/local/bin/plexus-agents

FROM debian:bookworm-slim
# ca-certificates: an issuer's JWKS and peers are reached over https/wss.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --home-dir /data --shell /usr/sbin/nologin app \
 && mkdir -p /data && chown app:app /data
COPY --from=build /usr/local/bin/plexus-agents /usr/local/bin/plexus-agents
USER app
WORKDIR /data
# Everything the service keeps: threads, runs and paired hosts (agents.db),
# sign-in sessions (agents.db.sessions) and the pushed UI's versions (ui.db).
VOLUME /data
ENV HOME=/data \
    AGENTS_DB=/data/agents.db \
    AGENTS_UI_DB=/data/ui.db \
    AGENTS_BIND=0.0.0.0 \
    AGENTS_PORT=4470 \
    PLEXUS_NO_REGISTRY=1 \
    RUST_LOG=info
EXPOSE 4470
STOPSIGNAL SIGTERM
LABEL org.opencontainers.image.title="agents"
ENTRYPOINT ["/usr/local/bin/plexus-agents"]
