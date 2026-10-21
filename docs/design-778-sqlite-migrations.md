# Schema migrations for remuda's SQLite databases

Status: design, not built yet. Follows #778 (the daemon owns SQLite).

## The ask

Owner: only the SQLite engine is needed, and upgrading and downgrading with
plain SQL is enough. This covers two kinds of database:

- **Mod databases**: `remuda.db.open(name)` opens `<state>/remuda/db/MOD/NAME.db`.
- **The VFS index**: `vfs/index.db`. Today it is `user_version` 1, plus a
  one-off import of `index.json` and a `meta` table (#794).

Every database is opened only by the daemon.

## What similar tools do

Tools surveyed: Alembic, Rails, Flyway, goose, golang-migrate, refinery,
rusqlite_migration, sqlx migrate, Django, and SQLite's own PRAGMAs. The
bundled SQLite is 3.53.2.

| Question | What they do | What we take |
|---|---|---|
| Version numbering | Timestamps (Rails, goose) or DAGs (Alembic, Django) fix merge races between many authors in one repo. golang-migrate, rusqlite_migration and sqlx use plain integers. | Contiguous integers 1..N per database. A mod's migrations have one author, so there are no merge races to solve. A gap or a duplicate is an error before the database is touched. |
| Up/down form | Paired files `N_x.up.sql` / `N_x.down.sql` (golang-migrate, sqlx), or markers in one file (goose). A missing down means irreversible (Rails `IrreversibleMigration`, Django `IrreversibleError`). | Paired files, with the down file optional. A missing down makes that step irreversible. |
| Where the version lives | A table (most tools), or `PRAGMA user_version` alone (rusqlite_migration). | `user_version` is the version. A table, `_remuda_migrations`, holds one row per applied step. |
| Edited files | Flyway, sqlx and refinery store a checksum of each applied step and refuse a mismatch. | Store the sha256 of each applied up file. A mismatch refuses the open with `drift:`. The fix is a new migration, never an edit. |
| Transactions | SQLite DDL is transactional. rusqlite_migration runs the whole pending range in one transaction and sets `user_version` in it. golang-migrate needs a `dirty` flag only because it writes the version in a separate transaction. | One `BEGIN IMMEDIATE` per run: apply every pending step, write the rows and `user_version`, then commit. A crash leaves the old schema and version or the new ones, never a mix, so no dirty flag is needed. |
| Foreign keys | `PRAGMA foreign_keys` does nothing inside a transaction. Runners turn it off first and run `foreign_key_check` before commit. | The runner does exactly that. A violation rolls everything back. |
| Statements a migration can't hold | `VACUUM` and `journal_mode` cannot run in a transaction, and writing `schema_version` corrupts the file. | Migration SQL runs under an authorizer. It refuses ATTACH, DETACH, transaction control, every PRAGMA except `foreign_key_check`, and any write to `_remuda_*` tables. |
| Downgrade that loses data | No tool detects it; that is the author's job. Backups are the usual safety net. | A downgrade runs only by explicit command. It first writes a private backup, `NAME.db.v<from>.bak`, through the SQLite backup API. |
| Older code meets a newer schema | rusqlite_migration (`DatabaseTooFarAhead`) and sqlx (`VersionMissing`) refuse. | Refuse with `too_new:`. Never open best-effort. |
| Rolling back with older code | No surveyed tool can: the down SQL ships only with the newer code. | Each applied step's down SQL is stored in `_remuda_migrations`. Any remuda build that has the runner can then step back without the newer mod. |
| Locking | sqlx's and golang-migrate's SQLite locks are no-ops or in-process only. | The daemon is the only opener, and `BEGIN IMMEDIATE` is SQLite's own write lock. |

## Mod databases

Migration files are shipped in the mod and declared in `assets`, as other
inert files are:

```
migrations/notes/0001_create.up.sql
migrations/notes/0001_create.down.sql
migrations/notes/0002_tags.up.sql      # no down file: step 2 is irreversible
```

The name pattern is `NNNN_slug.up.sql` and `NNNN_slug.down.sql`. The numbers
are contiguous from 1. An empty file is an error; an intentional no-op holds
an SQL comment.

```lua
local db = assert(remuda.db.open("notes", { migrations = true }))
```

`migrations = true` loads `<mod>/migrations/NAME/`. A mod can also pass an
array of `{ name = "create", up = "...", down = "..." }` tables, where step
`i` is entry `i`. On open:

1. Read and check the list. Any gap, duplicate, empty file or bad name fails
   with `migration:`.
2. If `user_version` is greater than N, refuse with `too_new: notes.db is at
   version 5, this mod knows 3`.
3. If any applied step's stored sha256 differs from the file, refuse with
   `drift: step 2 changed after it was applied`.
4. Apply steps `user_version+1 ..= N` in one transaction with foreign keys
   off, then run `foreign_key_check`, write the rows and `user_version = N`,
   and commit. The time limit for this run is the ceiling (10 s). The size
   limit is the handle's.
5. Return the handle. The handle may not write `user_version` or the
   `_remuda_migrations` table.

When `open` is called without `migrations` on a database that has applied
steps, it is refused with `too_new:`. Old code that does not know about the
schema cannot write to it.

## Command line (runs in the daemon, like `remuda vfs`)

- `remuda db status [MOD]`: each database's version, applied steps, which
  steps can be undone, and drift, if the mod's files are installed.
- `remuda db upgrade MOD NAME`: apply the installed files, the same as `open`
  does.
- `remuda db downgrade MOD NAME VERSION [--yes]`: take a backup, then run the
  stored down SQL from the current version down to VERSION in one
  transaction. It is refused before anything runs if any step in that range
  is irreversible. Without `--yes`, it prints what it would run and stops.

With no daemon running, every verb prints one line and exits 1, the same as
`vfs`.

## The VFS index

**Decision:** the VFS index moves to the same runner, with its steps compiled
in.

- Step 1 is today's table. It records `user_version` 1, so builds from #791
  and #794 still open the file.
- The `index.json` import and re-merge stay as code that runs after the
  migrations, because they are data and not schema.
- An older build already refuses a newer `index.db`, with "upgrade remuda".
- With down SQL stored in the file, a later build can step `index.db` back
  before a downgrade, in the same way as a mod database.

## Tests in the implementation PR

- Unit tests:
  - Contiguity, gap and duplicate errors.
  - Drift.
  - `too_new`.
  - An irreversible step blocks a downgrade before anything runs.
  - A foreign key violation rolls back.
  - The authorizer refuses PRAGMA, ATTACH and BEGIN in migration SQL.
  - Stored down SQL works without the files.
- Crash midway: a child process applies a long multi-step run and is killed
  with `kill -9`. On reopen, the schema and version are both the old ones,
  and the next open applies all the steps.
- Older code meets a newer schema: a database migrated to step 3 is opened
  with steps 1..2 (`too_new`) and with no migrations (`too_new`). For
  `index.db`, a `user_version` 2 file is refused by today's code with
  "upgrade remuda".
- A live CLI test of `status`, `downgrade` without `--yes`, and `downgrade
  --yes`, including the backup file and its 0600 mode.
