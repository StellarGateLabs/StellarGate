# Base images are pinned by digest so silent upstream updates cannot change the
# build. Keep the rust tag in sync with rust-toolchain.toml / Cargo.toml MSRV.
# To refresh a pin:
#   docker buildx imagetools inspect rust:1.94-bookworm --format '{{.Manifest.Digest}}'
#   docker buildx imagetools inspect debian:bookworm-slim --format '{{.Manifest.Digest}}'
# Then update the sha256 in the matching FROM line below.

# ── Stage 1: dependency cache via cargo-chef ─────────────────────────────────
FROM rust:1.94-bookworm@sha256:6ae102bdbf528294bc79ad6e1fae682f6f7c2a6e6621506ba959f9685b308a55 AS chef
# cargo-chef is pinned to an exact version so builds are reproducible and do
# not silently pick up a newly published release. Bump deliberately.
ARG CARGO_CHEF_VERSION=0.1.71
RUN cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies only — cached unless Cargo.toml/Cargo.lock change
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked

# ── Stage 2: slim runtime image ───────────────────────────────────────────────
FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS runtime

ARG VERSION=dev
ARG VCS_REF=unknown

LABEL org.opencontainers.image.description="Non-custodial payment gateway API for accepting and settling payments on the Stellar network" \
    org.opencontainers.image.source="https://github.com/StellarGateLabs/StellarGate" \
    org.opencontainers.image.licenses="MIT" \
    org.opencontainers.image.version="${VERSION}" \
    org.opencontainers.image.revision="${VCS_REF}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -r -u 1001 -U stellargate \
    && mkdir /data \
    && chown stellargate:stellargate /data

COPY --from=builder /app/target/release/stellargate /usr/local/bin/stellargate

USER stellargate

ENV DATABASE_URL=sqlite:///data/stellargate.db

EXPOSE 3000

# The binary checks its own health (`stellargate healthcheck [path]`), so the
# runtime image needs no HTTP client of its own. Compose files can override
# `test:` to point at a different path (e.g. `/ready`) or timing.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/stellargate", "healthcheck", "health"]

CMD ["/usr/local/bin/stellargate"]
