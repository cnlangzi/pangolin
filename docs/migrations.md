# Database Migrations

Pangolin uses [refinery](https://github.com/rust-db/refinery) to manage SQLite schema migrations. Migrations are embedded into the `pangolin-ngx` binary at compile time and run automatically on startup.

## How migrations work

- **Location**: `crates/pangolin-core/migrations/`
- **Naming**: `V{version}__{description}.sql` (e.g., `V1__initial_schema.sql`, `V2__merge_tokens_into_tun.sql`)
- **Tracking**: Applied migrations are recorded in the `refinery_schema_history` table with a checksum
- **Startup**: `pangolin-ngx` runs pending migrations on every start; already-applied migrations are skipped

## Migration files

Each `.sql` file in `migrations/` represents one schema change:

```
crates/pangolin-core/migrations/
├── V1__initial_schema.sql
└── V2__merge_tokens_into_tun.sql
```

Version numbers must be sequential integers. The description (after `__`) is human-readable and appears in the history table.

## Common scenarios

### Fresh database (development)

When you run `pangolin-ngx` for the first time:

1. It creates `pangolin.db` (in the current working directory)
2. Runs all migrations in order (V1, V2, ...)
3. Records each in `refinery_schema_history`

No manual intervention needed.

### Adding a new migration

1. Create `crates/pangolin-core/migrations/V{N+1}__your_description.sql`
2. Write the SQL statements (DDL only; data changes belong in application code)
3. Rebuild `pangolin-ngx` (migrations are embedded at compile time)
4. Restart the binary — refinery runs the new migration automatically

Example:

```sql
-- V3__add_tun_metadata.sql
ALTER TABLE tun ADD COLUMN region TEXT;
ALTER TABLE tun ADD COLUMN tags TEXT;
```

### Migration checksum mismatch error

If you see:

```
Error: migration: applied migration V2__merge_tokens_into_tun is different than filesystem one V2__merge_tokens_into_tun
```

This means the content of a migration file **changed after it was applied**. Refinery tracks each migration's checksum to prevent accidental schema drift.

**Why this happens:**
- You edited a migration file that was already applied to the database
- You switched git branches with different migration history
- The database was created from an older version of the code

**Solutions:**

#### Development environment (safe to lose data)

Delete the database and restart:

```bash
rm pangolin.db pangolin.db-shm pangolin.db-wal
make start-ngx
```

The binary will recreate the database with the current migrations.

#### Production environment (must preserve data)

**DO NOT delete the database.** Instead:

1. Identify which migration changed:
   ```bash
   sqlite3 pangolin.db "SELECT version, description, checksum FROM refinery_schema_history;"
   ```

2. If the change was intentional and backwards-compatible:
   - Create a **new** migration (V{N+1}) with the additional changes
   - Never modify an applied migration

3. If the change was accidental:
   - Revert the migration file to its original content
   - Rebuild and restart

4. If you need to forcibly update the checksum (dangerous):
   ```sql
   -- Recalculate checksum for the current file content
   -- This is a last resort; you are asserting the schema is correct
   UPDATE refinery_schema_history 
   SET checksum = <new_checksum>
   WHERE version = 2;
   ```

   Refinery checksums are `i64` hashes of file bytes. There is no
   CLI to compute them. If you must update a checksum, use `sqlite3`
   to inspect the current value, verify the schema matches your
   migration file exactly, then manually update the row. This is
   dangerous and should be a **last resort** — you are bypassing
   refinery's integrity check.

### Rolling back a migration

**Refinery does not support automatic rollback.** Once a migration runs, it is permanent.

If you need to undo a migration:

1. Write a **new** migration (V{N+1}) that reverses the change
2. Test it carefully — `DROP COLUMN` loses data irreversibly

Example:

```sql
-- V4__revert_tun_metadata.sql
-- Undo V3: remove the columns added in V3
ALTER TABLE tun DROP COLUMN region;
ALTER TABLE tun DROP COLUMN tags;
```

### Checking migration status

List applied migrations:

```bash
sqlite3 pangolin.db "SELECT version, description, applied_on FROM refinery_schema_history ORDER BY version;"
```

List migration files on disk:

```bash
ls -1 crates/pangolin-core/migrations/
```

If the database has fewer rows than files, those migrations are pending and will run on next startup.

## Best practices

1. **Never edit an applied migration** — refinery will reject it. Always create a new V{N+1} file.
2. **Test migrations on a copy** of production data before deploying.
3. **Idempotent DDL** when possible: `CREATE TABLE IF NOT EXISTS`, `ALTER TABLE … ADD COLUMN … DEFAULT …` (so existing rows pass the constraint).
4. **Separate data changes** from schema changes — migrations are DDL; use the admin API or a one-time script for bulk `UPDATE`s.
5. **Commit migrations with code** — PR #23 merged V2 with the code that expects the new schema, ensuring they stay in sync.
6. **Version numbers are global** — you cannot have two V3 migrations. Coordinate with your team if working on concurrent schema changes.

## Migration history

| Version | Description               | PR   | Notes |
| ------- | ------------------------- | ---- | ----- |
| V1      | `initial_schema`          | —    | Sites, domains, tun (v1: separate `tokens` table), certs, dns_providers |
| V2      | `merge_tokens_into_tun`   | #23  | Collapsed `tokens` into `tun` (1:1 relationship enforced in code anyway); `tun.token` + `tun.expires_at` added |

## Troubleshooting

### "no such table: refinery_schema_history"

The database exists but migrations never ran. Possible causes:

- You created `pangolin.db` manually (e.g., via `sqlite3 pangolin.db "VACUUM;"`)
- The `pangolin-ngx` binary panicked before `db::migrate()` completed

**Fix**: delete `pangolin.db` and let `pangolin-ngx` recreate it.

### "database is locked"

Another process (or a stale `-shm`/`-wal` file from a crash) holds the database lock.

**Fix**:
```bash
pkill -f pangolin-ngx
rm pangolin.db-shm pangolin.db-wal
# Then restart
```

### Migrations run on every startup (slow)

This is expected behavior — refinery checks `refinery_schema_history`
on every start. Skipping already-applied migrations is fast (a `SELECT`
per migration, typically <10 ms total for a few migrations). If you
see actual SQL execution on every start, the migrations are not being
recorded — check that `db::migrate()` completes without error.

## Database location

`pangolin-ngx` opens `pangolin.db` in the current working directory
(see `crates/ngx/src/main.rs`, `let db_path = PathBuf::from("pangolin.db")`).
There is currently **no environment variable or CLI flag to override
the path** — run the binary from the directory that holds the database
you want to use, or symlink/`cd` into it from your service unit.

If you need per-environment databases (dev, staging, prod), run each
binary in a different working directory. Migrations apply to each
database independently — starting a new staging instance from a prod
snapshot will only run migrations newer than that snapshot's
`refinery_schema_history`.

## Further reading

- [Refinery docs](https://github.com/rust-db/refinery)
- [SQLite ALTER TABLE](https://www.sqlite.org/lang_altertable.html)
- `crates/pangolin-core/src/db.rs` — the `migrate()` function and schema docs
