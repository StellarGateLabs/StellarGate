# Contributing to StellarGate

Thanks for your interest in contributing! StellarGate is a Rust payment
gateway API for the Stellar network, and it's open to community
contributions — including through the Wave Program (scoped issues tagged for
outside contributors).

This document covers how to set up the project, the standards your PR is
expected to meet, and how to submit changes.

## Code of Conduct

This project follows the [Code of Conduct](CODE_OF_CONDUCT.md). By
participating, you're expected to uphold it.

## Before You Start

- **Look for an existing issue** before starting work. If one doesn't exist
  for what you want to do, open one first and describe the problem/feature —
  this avoids duplicate work and lets maintainers weigh in on approach before
  you invest time.
- **Say you're working on it.** Comment on the issue (or ask a maintainer to
  assign it to you) so two people don't build the same thing in parallel.
- For anything nontrivial (new endpoints, schema changes, changes to webhook
  signing/verification, changes to the SSRF guard), it's worth sketching your
  approach in the issue before writing code — security- and payment-adjacent
  logic gets extra scrutiny in review.

**Work tracking:** this repository uses GitHub Issues as its sole tracker. Ad-
hoc checklists and TODO files should not be committed to the repository — they
create ambiguity about where work is tracked and can become stale. Design
rationale and decision records belong in code comments, issue discussions, or
`CHANGELOG.md`.

## Development Setup

### Prerequisites

- Rust 1.75+ — [install via rustup](https://rustup.rs)

### Getting started

```bash
git clone https://github.com/<your-fork>/StellarGate.git
cd StellarGate

cp .env.example .env
# Edit .env — at minimum you'll want STELLAR_NETWORK=testnet and a
# STELLAR_GATEWAY_PUBLIC key if you're exercising the Horizon listener.

cargo build
cargo test
```

See the [README](README.md) for the full environment variable reference,
API documentation, and project structure.

### Running locally

```bash
cargo run
# or, without installing Rust:
docker compose up --build
```

## Making Changes

1. **Fork the repo** and create a branch off `main`:
   ```bash
   git checkout -b feat/short-description
   ```
   Use a prefix that matches the change: `feat/`, `fix/`, `docs/`, `test/`,
   `refactor/`, `chore/`.
2. **Write the code.** Keep changes scoped to the issue you're addressing —
   unrelated cleanup or refactors belong in a separate PR.
3. **Add or update tests.** New endpoints, validation rules, and bug fixes
   should come with test coverage in `tests/api_tests.rs` (integration) or
   inline `#[cfg(test)]` modules (unit). If you're fixing a bug, add a test
   that fails before your fix and passes after.
4. **Add the statement to `db::migrate` if you touch the schema.** `src/db.rs`'s
   `db::migrate` is the only schema definition in this repository — there is
   no `migrations/` directory. Add your `CREATE TABLE IF NOT EXISTS` /
   `ALTER TABLE ... ADD COLUMN` statement there, keep it idempotent (it runs
   on every startup of every existing deployment), and update
   `tests/schema_snapshot.sql` to match (`cargo test --test
   schema_snapshot_test -- --nocapture` prints the exact text to paste in on
   a mismatch). See "Database Migrations" in the README.
5. **Update docs.** If you change environment variables, API request/response
   shapes, or webhook payloads, update the README and (for `.env.example`-
   affecting changes) `.env.example` in the same PR.

## Before Opening a Pull Request

Run the same checks CI runs, so review isn't spent on formatting/lint churn:

```bash
cargo fmt --check      # formatting
cargo clippy --all-targets -- -D warnings   # lints, warnings-as-errors
cargo test              # full test suite
```

If you touched `Cargo.toml`/`Cargo.lock`, also expect the supply-chain
workflow (`cargo audit` via `cargo-deny`) to run in CI — check `deny.toml` if
you're adding a new dependency with a license or advisory it doesn't already
allow.

## Toolchain & CI Policy

Blocking CI jobs (like formatting and clippy) are strictly tied to our pinned MSRV (1.94) as defined in `rust-toolchain.toml`. This ensures that your local `cargo clippy` and `cargo fmt` results perfectly match CI. An advisory lint job runs on the latest stable toolchain to catch upcoming lints, but it is set to `continue-on-error: true` and will not block your pull request.

## Dependency Upgrade Policy

Dependency upgrades should be small, reviewable, and documented. For routine
patch or minor updates, keep the lockfile change together with any required
source change and add a short `CHANGELOG.md` entry when runtime behavior,
minimum supported Rust version, build tooling, TLS roots, or deployment
requirements change. Major upgrades need an issue first, because they can alter
security posture, API behavior, or production build requirements.

When an upgrade changes the Rust toolchain, Docker image, system packages, or
release workflow, update `README.md`, `DEPLOYMENT.md`, and the relevant example
environment files in the same PR.

## Commit Messages

Keep commits focused and messages descriptive of *why*, not just *what*.
Conventional prefixes (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`,
`chore:`) are welcome but not required — clarity matters more than format.

## Opening a Pull Request

1. Push your branch and open a PR against `main`.
2. Reference the issue you're closing, e.g. `Closes #123`.
3. Describe **what changed and why**, and how you tested it (which
   `cargo test` cases cover it, or manual steps if it's not easily testable
   automatically).
4. Ensure CI (fmt, clippy, test, supply-chain audit) passes — PRs with
   failing checks won't be merged.
5. Respond to review feedback; it's normal to go through a round or two,
   especially for anything touching payment verification, webhook signing,
   or auth.

## Security-Sensitive Changes

Please do **not** open a public PR or issue for a security vulnerability —
see [SECURITY.md](SECURITY.md) for how to report those privately. Ordinary
hardening improvements (e.g. tightening validation, adding a missing check)
are welcome as normal PRs; use your judgment about whether a change reveals
an exploitable gap that should be reported privately first instead.

## Style Notes

- Follow existing patterns in the module you're editing (see `src/`'s module
  layout in the README's "Project Structure" section) rather than
  introducing a new style or abstraction for a single change.
- Prefer explicit error handling over `unwrap()`/`expect()` outside of tests
  and startup-time config validation.
- Money amounts are handled as stroops via `src/money.rs` — don't introduce
  floating-point arithmetic for amounts.

## Questions

If anything here is unclear, or an issue's scope is ambiguous, ask on the
issue itself before starting — it's cheaper than redoing work after the
fact.
