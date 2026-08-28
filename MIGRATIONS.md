# Database Migrations

## How schema changes work

StellarGate does **not** use a migration runner or a numbered `migrations/`
directory. Schema evolution is handled entirely in [`src/db.rs`](src/db.rs) by
the `db::migrate` function, which is called once from `main` before the HTTP
listener binds.

Every statement in `db::migrate` is written to be **safe to re-run on every
boot**:

- Tables and indexes use `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT
  EXISTS`.
- New columns on existing tables are added by first probing
  `pragma_table_info(...)`, then running `ALTER TABLE ... ADD COLUMN` only if
  the column is absent.
- One-time data backfills (filling `asset_issuer` from `ACCEPTED_ASSETS`,
  normalising pre-RFC 3339 timestamps, populating `processed_transactions`
  from legacy rows) are also written to be idempotent: they act only on rows
  that need the fix, so a second run is a no-op.

There is no version table. Nothing is recorded as "applied". The entire
sequence runs unconditionally on every startup.

## Why there are no down migrations

The `migrations/` directory referenced in issue #454 does not exist and never
did — it was removed (issue #308) because the numbered `.sql` files it
contained were never executed by anything (`sqlx::migrate!` was never called),
had silently drifted to the point of missing entire tables and columns, and
looked authoritative enough to mislead future contributors. `db::migrate` in
`src/db.rs` is the single, canonical schema definition.

**Down migrations are deliberately not provided**, and this is the correct
policy for this project for the following reasons:

### 1. The target environment is a single-node SQLite deployment

StellarGate targets a single Always-Free VM running one container with one
SQLite file on a named volume. There is no replica to keep in sync, no
migration coordinator, and no mechanism for rolling a schema change back
independently of the application binary. A "rollback" in this environment
means restoring the pre-upgrade binary **and** the pre-upgrade database backup
— the two are versioned together, not independently.

### 2. Every schema change must be backward-compatible

Because `db::migrate` runs on every boot and is idempotent, the rule for
adding schema is:

- **New table**: `CREATE TABLE IF NOT EXISTS` — a no-op on upgrade.
- **New column**: probe `pragma_table_info`, then `ALTER TABLE ... ADD COLUMN`
  — a no-op on subsequent boots.
- **New index**: `CREATE INDEX IF NOT EXISTS` — a no-op on subsequent boots.

Changes that are *not* backward-compatible (dropping a column, renaming a
column, changing a column type, adding a `NOT NULL` constraint without a
default) cannot currently be expressed without violating the idempotency
requirement. If such a change is needed, resolve
[issue #268](https://github.com/StellarGateLabs/StellarGate/issues/268)
(adopting `sqlx::migrate!` with recorded schema versions) first.

### 3. The schema snapshot test catches drift

`tests/schema_snapshot_test.rs` runs `db::migrate` against an in-memory
database and asserts the result matches `tests/schema_snapshot.sql` exactly.
A schema change that is not reflected in the snapshot file fails CI, closing
the same silent-drift failure mode the old `migrations/` directory had.

## Making a schema change

1. Add the statement to `db::migrate` in `src/db.rs`, keeping it idempotent.
2. For a new column on an existing table, follow the `pragma_table_info` probe
   pattern already used for `expires_at` and `event_type`. SQLite rejects a
   non-constant `DEFAULT` on `ALTER TABLE ... ADD COLUMN`, so add the column
   nullable and backfill it in a second statement.
3. Run `cargo test` — `tests/schema_snapshot_test.rs` will fail with the new
   schema's exact text, ready to paste into `tests/schema_snapshot.sql`.
4. Review and commit the snapshot diff alongside the `db.rs` change.

## Rollback procedure

Since there are no down migrations, rollback is:

1. Restore the database volume from the backup taken immediately before the
   upgrade (see [DEPLOYMENT.md](DEPLOYMENT.md) for the backup procedure).
2. Roll the application binary back to the previous version.

A `CREATE TABLE IF NOT EXISTS` or `ADD COLUMN IF NOT EXISTS` that ran on the
new version is harmless to the old binary — it will simply not use the new
table or column. A data backfill that ran is also harmless: the old binary
reads the same rows it always did. The only scenario that requires a backup
restore is a change that removed or altered existing data, which the
backward-compatibility rule above prevents.
