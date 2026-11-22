# Contributing

Thanks for taking the time to contribute. This document covers the conventions we expect in pull requests.

## Getting started

1. Fork the repository and create a topic branch off `main`.
2. Make your change, keeping the diff focused on a single concern.
3. Run the test suite locally before opening a PR.
4. Open a pull request describing the change and the issue it addresses.

## Regression test per issue

Every fix for a closed security or correctness issue must land with a test that pins the behaviour and references the issue number. This is how we prevent a future revert from silently deleting both the fix and its coverage.

- **Name tests after the issue they pin.** For example:

  ```rust
  #[tokio::test]
  async fn issue_254_idempotency_key_rejects_different_body() {
      // ...
  }
  ```

  Alternatively, keep a descriptive name and add a doc comment:

  ```rust
  /// Regression: #254
  #[test]
  fn idempotency_key_rejects_different_body() {
      // ...
  }
  ```

- **Prefer the guard file.** `tests/regressions.rs` holds one fast, black-box test per closed security or correctness issue, exercising the public router only. A single file is much harder to lose in a conflict resolution than tests spread across many modules. Add new regression tests there unless the behaviour genuinely needs a unit test inside `src/`.

- **Do not delete tests without approval.** CI runs a test-count guard that fails a pull request when the number of `#[test]` / `#[tokio::test]` functions drops by more than the allowed threshold. If a test removal is intentional, apply the `test-removal-approved` label to the PR and explain the removal in the description.

## Pull request checklist

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

## Working on the dashboard

The dashboard is plain HTML, CSS and ES modules in `static/`, compiled into the
binary with `include_str!`, so there is no build step.

- Edit `static/dashboard.js` (controller), `static/state.js` (view state, URL
  hash), `static/format.js` (pure helpers) and `static/dashboard.html`/`.css`.
- Run `cargo run`, open `http://localhost:3000/dashboard` and reload after each
  edit (rebuild with `cargo run` since assets are embedded).
- Quick checks: `node --check static/dashboard.js`,
  `node --test static/tests/*.test.js` and
  `cargo test --test dashboard_asset_tests`.
- Keep the API key out of URLs, logs and the console; only filters and
  `payment=<id>` may go in the hash.

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

## Code Coverage

CI measures test coverage on every run with
[`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) and uploads the
LCOV report as a build artifact and to Codecov, so the report is reachable
from the pull request. Coverage is **report-only** — there is no absolute
threshold gate. Instead, Codecov flags a regression when a PR drops coverage
relative to the recorded baseline, so a change that leaves new paths
unexercised is visible in review without encouraging tests written purely for
the metric.

To produce the same report locally:

```bash
The CI job installs `cargo-llvm-cov` via `taiki-e/install-action` (prebuilt binary).
Locally: `cargo install cargo-llvm-cov --locked`, or use the same install action output.
cargo llvm-cov --locked --all-features --lcov --output-path lcov.info
```

For a human-readable summary instead of LCOV, run
`cargo llvm-cov --locked --all-features --summary-only`, or open an HTML
report with `cargo llvm-cov --locked --all-features --html --open`. Use the
report to see which paths your change leaves unexercised — especially in
security-sensitive areas like the SSRF guard, webhook delivery, and
retention error handling.

## Toolchain & CI Policy

Blocking CI jobs (like formatting and clippy) are strictly tied to our pinned MSRV (1.94) as defined in `rust-toolchain.toml`. This ensures that your local `cargo clippy` and `cargo fmt` results perfectly match CI. An advisory lint job runs on the latest stable toolchain to catch upcoming lints, but it is set to `continue-on-error: true` and will not block your pull request.

## Required Status Checks

`main` is protected: a pull request cannot be merged until all of the
following required status checks have run **and passed** on the PR's head
commit:

- `test (1.94)`
- `test (stable)`
- `fmt / clippy`
- `openapi lint`
- `deploy asset consistency`
- `dashboard static checks`
- `dashboard unit tests`

A PR with failing **or missing** required checks cannot be merged into
`main`. "Missing" matters: if a check never ran, it does not count as passing.
If the merge button is disabled or the checks list is empty, do not merge —
figure out why CI didn't run first.

Branch protection also requires branches to be **up to date before merging**,
so a PR is tested against the `main` it will actually merge into. If `main`
has moved since your last push, update your branch (merge or rebase `main`)
and let CI re-run before merging. Most breakage we've seen came from
merge-resolution drift, not from the PRs themselves.

### Fork PRs: approve workflow runs first

PRs opened from forks do not run Actions automatically — a maintainer must
approve the workflow run (the **"Approve and run"** button on the PR's checks
section) before CI executes. Until that approval happens, the PR shows **zero
checks**, and merging it would bypass every required check above.

**Maintainers: approve fork workflow runs before reviewing or merging.** A
fork PR with no checks reported is not ready for review — approve the run,
wait for the required checks to go green, and only then review and merge.

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
