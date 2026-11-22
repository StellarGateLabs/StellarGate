# Deployment

This document describes how to deploy the gateway in production.

## Health and readiness probes

- `GET /health` — liveness. Returns `200` while the process is running.
- `GET /ready` — readiness. Pings the database and checks Horizon reachability.

### Do not proxy probes publicly

`/ready` and `/health` are intended for the orchestrator / load balancer only.
They are unauthenticated and are exempt from the API rate limiter, so they must
not be reachable from the public internet.

- Point probes at the container directly (e.g. `http://app:3000/ready` on the
  Docker network) or at an internal-only listener, not through the public
  reverse proxy.
- If you front the gateway with Caddy (see `deploy/Caddyfile`), block
  `/ready` and `/health` on the public site block (the stock Caddyfile does
  this).

Exposing `/ready` publicly lets anonymous callers drive outbound Horizon
requests and can get the gateway's egress IP rate-limited by Horizon, which
degrades payment detection.

## Shutdown grace

The gateway honours `SHUTDOWN_GRACE_SECS` (default 30) while draining HTTP and
background tasks. Docker Compose should set `stop_grace_period` a few seconds
**above** that budget so the process is not SIGKILL'd mid-drain — the root
`docker-compose.yml` and `deploy/docker-compose.prod.yml` use `35s`.

## Resource limits

`deploy/docker-compose.prod.yml` sets CPU and memory limits sized for a small
always-on VM (see comments in that file). Dev defaults live in the root
`docker-compose.yml` and can be overridden with `COMPOSE_CPU_LIMIT` /
`COMPOSE_MEMORY_LIMIT` / `COMPOSE_MEMORY_RESERVATION`.

## Backup procedure

The SQLite database lives in the `stellargate_data` volume (`/data` in the app
container). To back up:

1. `docker compose … stop app` (or pause traffic at the proxy).
2. Copy `/data/stellargate.db` and any `-wal`/`-shm` siblings from the volume.
3. Start the stack again.

Restore by placing the files back into a fresh volume before first boot.

## Forward-only migrations

Schema changes are applied only through idempotent statements in `db::migrate`
on every startup — there is no down migration. Roll forward with a fixed
release image; see [MIGRATIONS.md](MIGRATIONS.md).

## Operating

- **Metrics:** scrape `GET /metrics` with `Authorization: Bearer <METRICS_TOKEN>` from a trusted network (blocked at the Caddy edge by default).
- **Logs:** container stdout; set `RUST_LOG` in `stellargate.env`. Structured JSON via `LOG_FORMAT=json` when supported by your release.

## Environment

See `.env.example` and `deploy/stellargate.env.example` for configuration.
`PORT` defaults to `3000`; the API listens on `BIND_ADDR` (default all IPv4
interfaces). Settings in a `.env` file are read at startup, including
`RUST_LOG` and `PORT` (used by `stellargate healthcheck`). Variables already
set in the process environment take precedence over `.env`.

## Upgrades and rollback

Pin `STELLARGATE_VERSION` in `deploy/stellargate.env` to a released GHCR tag,
then `systemctl restart stellargate` (or `docker compose up -d --pull always`).
