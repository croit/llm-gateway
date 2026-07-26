# Migrations

`sqlx::migrate!()` runs every file in this directory in numeric
order at gateway startup. The first run inserts a row per
file into the `_sqlx_migrations` table, recording the migration
number, a checksum of the file bytes, and the timestamp it was
applied.

## Treat applied migrations as immutable

On every subsequent startup sqlx re-hashes each migration file
and compares it to the recorded checksum. **Any change — including
whitespace and comment edits — bumps the hash and the gateway
refuses to boot:**

```
Error: running migrations
Caused by: migration N was previously applied but has been modified
```

This is by design: the recorded checksum is sqlx's way of catching
"someone hand-edited migration history out from under the
production DB."

### Rules

1. **Never edit a `.sql` file that has been committed to `main`.**
   That includes comments. Once it's merged, assume it's running
   in production and is locked.
2. **Need to change something?** Add a new numbered migration
   (`000N_describe_change.sql`) with the schema delta. SQLite can
   do most things with `ALTER TABLE`; the rest goes through the
   `CREATE TABLE new` → `INSERT ... SELECT` → `DROP TABLE old` →
   `ALTER TABLE new RENAME` dance.
3. **Need to update a comment?** Put the prose in the migration's
   *commit message*, or in this README — not in the `.sql`.
4. **In-flight on a branch, not yet merged?** Editing your own
   migration is fine as long as the file hasn't reached prod yet.
   The moment it lands on `main`, rule (1) kicks in.

### Recovering from an accidental edit

If the boot is already broken by a checksum mismatch and you
*haven't* changed the SQL itself (only comments / formatting),
the quickest path is to restore the file to its pre-edit content
via `git show <prior-commit>:crates/gateway/migrations/000N_….sql`
and commit that. The DB never needed migrating; only the file
had to match what was applied.

If the SQL *did* change, do not retroactively rewrite history —
instead, add a follow-up migration that brings the schema to the
desired state. Operators running the previous version need the
file to keep matching their `_sqlx_migrations` row.
