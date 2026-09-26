# Base images are pinned by digest so silent upstream updates cannot change the
# build. Keep the rust tag in sync with rust-toolchain.toml / Cargo.toml MSRV.
# To refresh a pin:
#   docker buildx imagetools inspect rust:1.94-bookworm --format '{{.Manifest.Digest}}'
#   docker buildx imagetools inspect debian:bookworm-slim --format '{{.Manifest.Digest}}'
# Then update the sha256 in the matching FROM line below.

# ── Stage 1: dependency cache via cargo-chef ─────────────────────────────────
FROM rust:1.94-bookworm@sha256:6ae102bdbf528294bc79ad6e1fae682f6f7c2a6e6621506ba959f9685b308a55 AS chef
RUN cargo install cargo-chef --locked
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

LABEL org.opencontainers.image.description="StellarGate payment gateway — runs as non-root uid 1001"

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
