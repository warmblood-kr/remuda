# Schema migrations: prior art survey for remuda (SQLite, daemon-only, plain SQL)

Date: 2026-10-22. Context: remuda bundles rusqlite 0.40.2 / libsqlite3-sys 0.38.2 = **SQLite 3.53.2**
(checked in `~/.cargo/registry/.../sqlite3.h`), so `RENAME COLUMN` (3.25), `DROP COLUMN` (3.35)
and `ALTER COLUMN … SET/DROP NOT NULL` (3.53) are all available.

Unverified against docs (from memory): Alembic's "Can't locate revision" text, goose's
`goose_db_version` table, Flyway `repair` details. The rest was checked against the cited docs/source.

## 1. SQLite engine facts that constrain any design

- **DDL is transactional.** CREATE/DROP/ALTER run in the normal write transaction and roll back with
  it ([lang_transaction](https://www.sqlite.org/lang_transaction.html)). `VACUUM` cannot run in a
  transaction; some PRAGMAs (e.g. `journal_mode`) don't take effect in one.
- **`PRAGMA foreign_keys` is a no-op inside a transaction** — "may only be enabled or disabled when
  there is no pending BEGIN or SAVEPOINT"; it is per connection ([pragma](https://www.sqlite.org/pragma.html#pragma_foreign_keys)).
  So a runner that wraps each migration in a transaction must toggle it *outside*, and a migration
  file that says `PRAGMA foreign_keys=OFF` silently does nothing.
- **`PRAGMA defer_foreign_keys=ON`** is the in-transaction alternative: checks are deferred to COMMIT;
  auto-reset at each COMMIT/ROLLBACK. Setting it to 0 resets FK tracking and can let violations commit.
- **`PRAGMA foreign_key_check`** returns one row per violation; `integrity_check` does *not* find FK
  errors. Runners run it before COMMIT when FKs were off.
- **ALTER TABLE limits** ([lang_altertable](https://www.sqlite.org/lang_altertable.html)): ADD COLUMN
  cannot add PK/UNIQUE, non-constant default, NOT NULL without default, STORED generated column;
  DROP COLUMN fails if the column is PK/UNIQUE, indexed, in a CHECK/FK/generated column/trigger/view.
  Anything else (type change, constraint change) needs the **12-step rebuild**: FKs off (outside tx) →
  BEGIN → save index/trigger/view SQL → `CREATE TABLE new_X` → `INSERT INTO new_X SELECT …` →
  `DROP TABLE X` → `ALTER TABLE new_X RENAME TO X` → recreate indexes/triggers/views →
  `PRAGMA foreign_key_check` → COMMIT → FKs back on. The docs explicitly warn **not** to rename the old
  table first (that rewrites FK/trigger/view references to the temp name).
- **`PRAGMA legacy_alter_table`** (default OFF) restores pre-3.25 RENAME semantics (references not
  rewritten). New apps should leave it off; the 12-step order above is correct with it off.
- **`PRAGMA user_version`**: 32-bit int at header offset 60, unused by SQLite, set inside a transaction
  atomically with the DDL. **`PRAGMA application_id`**: 32-bit int at offset 68 identifying the file
  format (so `file(1)` and the app can recognise "this is ours"). `PRAGMA schema_version` is SQLite's
  own counter — never write it (corruption risk; no-op in defensive mode).
- **Concurrency**: `BEGIN IMMEDIATE` takes the write lock up front (or fails `SQLITE_BUSY`), so one
  writer at a time is guaranteed by the engine itself; no advisory-lock table is needed.

## 2. Tool-by-tool

### Alembic (Python/SQLAlchemy)
- **Versions**: opaque hex revision IDs; ordering via `down_revision` pointers (a DAG). Branches and
  multiple heads are detected (`upgrade head` fails with "Multiple head revisions"); `alembic merge`
  joins them ([branches](https://alembic.sqlalchemy.org/en/latest/branches.html)).
- **Up/down**: one Python file with `upgrade()`/`downgrade()`.
- **State**: `alembic_version` table holding the current head(s) only (one row per head). No checksums.
- **SQLite**: "batch mode" (`batch_alter_table`, `render_as_batch=True`) implements move-and-copy
  (create new, `INSERT…SELECT`, drop, rename) when anything beyond `add_column` is needed; FKs must be
  disabled for tables referenced by the rebuilt one; unnamed constraints are lost unless named
  ([batch](https://alembic.sqlalchemy.org/en/latest/batch.html)).
- **Mixed versions**: an unknown revision in `alembic_version` → "Can't locate revision" error (refuses).

### Rails / ActiveRecord
- **Versions**: UTC timestamps `YYYYMMDDHHMMSS_name.rb`. **Out-of-order is allowed**: any version not in
  `schema_migrations` runs, even if older than applied ones ([guide](https://guides.rubyonrails.org/active_record_migrations.html)).
- **Up/down**: `change` (auto-reversible), `reversible`, or explicit `up`/`down`; irreversible down
  raises `ActiveRecord::IrreversibleMigration`.
- **State**: `schema_migrations(version)` — set of applied versions, no checksum. Rule: never edit a
  shared migration; write a new one.
- **Tx**: one transaction per migration on DDL-transactional DBs; `disable_ddl_transaction!` opts out.
- `schema.rb`/`structure.sql` snapshot lets fresh DBs load the final schema instead of replaying.

### Flyway
- **Versions**: `V<ver>__desc.sql` (dotted versions), undo `U<ver>__desc.sql` (Teams/Enterprise only),
  repeatable `R__desc.sql` ([undo prefix](https://documentation.red-gate.com/fd/flyway-undo-sql-migration-prefix-setting-277579046.html)).
  Out-of-order off by default; a new lower version is "ignored" unless `outOfOrder=true`.
- **State**: `flyway_schema_history` (version, checksum, success, installed_by, …).
- **Drift**: `validate` compares checksums; `CHECKSUM_MISMATCH` → revert the file or `repair`
  ([error codes](https://documentation.red-gate.com/fd/validate-error-codes-287015530.html)).
- **Failure**: migration marked failed, rolled back if the DB allows; on non-transactional DDL you clean
  up by hand then `repair` (removes failed rows, realigns checksums). Philosophy (and refinery's):
  undo is mostly a new forward migration.

### goose (Go)
- **Versions**: timestamps by default, `-s` for sequential; recommended **hybrid**: timestamps in dev,
  `goose fix` renumbers to sequential before release (keeps order) ([overview](https://pressly.github.io/goose/blog/2022/overview-sql-file/)).
  `-allow-missing` / `WithAllowMissing()` applies skipped (out-of-order) versions.
- **Up/down**: **single SQL file with markers** `-- +goose Up` / `-- +goose Down`;
  `StatementBegin/End` for bodies containing `;` (triggers); `-- +goose NO TRANSACTION` per file
  ([annotations](https://pressly.github.io/goose/documentation/annotations/)).
- **State**: `goose_db_version` table (append-only log of version_id, is_applied). No checksums.
- **Tx**: one transaction per file by default.

### golang-migrate
- **Versions**: `{uint64}_{title}.up.sql` + `.down.sql` (paired files); any increasing ints, counters or
  timestamps ([MIGRATIONS.md](https://github.com/golang-migrate/migrate/blob/master/MIGRATIONS.md)).
  No out-of-order. Advice: test up→down→up; empty irreversible files should hold a comment (0-byte
  file can error).
- **State**: `schema_migrations(version, dirty)` — single row, no history, no checksum.
- **Dirty flag**: set *before* running a migration, cleared after. On failure "Dirty database version N.
  Fix and force version." — all further runs refuse until a human runs `force V`
  ([GETTING_STARTED](https://github.com/golang-migrate/migrate/blob/master/GETTING_STARTED.md)).
- **SQLite driver**: wraps each migration in an implicit tx (so files must not contain BEGIN/COMMIT);
  `x-no-tx-wrap=true` hands tx control to the file. Its `Lock` is an in-process `atomic.Bool` only
  ([sqlite3.go](https://github.com/golang-migrate/migrate/blob/master/database/sqlite3/sqlite3.go)).
  Note the version row is written in a *separate* tx from the migration — that's why the dirty flag
  exists; on SQLite it's avoidable by writing version in the same tx.

### refinery (Rust)
- **Versions**: `V{n}__name.sql` strict, `U{n}__name.sql` "unversioned" (may be non-contiguous / run
  after higher ones). i32 (or i64 feature; must pick before first use — switching breaks checksums).
- **No down migrations** (Flyway philosophy) ([README](https://github.com/rust-db/refinery)).
- **State**: `refinery_schema_history` with checksum; `set_abort_divergent` (applied file changed) and
  `set_abort_missing` (applied file gone).
- **Tx**: per-migration by default; `set_grouped(true)` = all pending in one tx.

### rusqlite_migration (Rust, SQLite-only)
- **Versions**: implicit — the index in a `Vec<M>`; `from_directory` feature loads `{id}-{name}/up.sql`
  (+ optional `down.sql`).
- **State**: **`PRAGMA user_version` only, no table.** Written in the same tx as the migrations.
- **Tx**: `to_latest`/`to_version` run *all* steps in one transaction and set user_version once; any
  error rolls back everything ([lib.rs](https://github.com/cljoly/rusqlite_migration/blob/master/rusqlite_migration/src/lib.rs)).
- **Down**: `M::up(..).down(..)`; downgrade checks the whole range first, `DownNotDefined` → nothing runs.
- **FKs**: per-migration `.foreign_key_check()` runs `PRAGMA foreign_key_check` inside the tx and rolls
  back on violation; docs say turn `foreign_keys` off *before* migrating, never from inside a migration;
  discourages PRAGMAs in migrations generally.
- **Newer DB**: user_version > known count → `MigrationDefinitionError::DatabaseTooFarAhead` (refuses).
- **Drift**: none (no checksums); "unspecified behaviour" if something else writes user_version.

### sqlx migrate (Rust)
- **Versions**: i64 prefix (timestamp default, sequential option); reversible pairs
  `<ver>_<desc>.up.sql`/`.down.sql`, or simple (up-only). Errors `VersionTooOld`/`VersionTooNew`
  guard ordering ([error.rs](https://github.com/launchbadge/sqlx/blob/main/sqlx-core/src/migrate/error.rs)).
- **State**: `_sqlx_migrations(version, description, installed_on, success, checksum BLOB, execution_time)`.
- **Drift**: applied migration with different checksum → `VersionMismatch`; applied version absent
  from source → `VersionMissing` (unless `set_ignore_missing(true)`) — i.e. **an older binary refuses a
  newer DB by default** ([migrator.rs](https://github.com/launchbadge/sqlx/blob/main/sqlx-core/src/migrate/migrator.rs)).
- **Dirty**: rows with `success=false` → `Dirty(v)`; only arises on non-transactional DDL backends.
  On SQLite the script + bookkeeping INSERT share one tx, so partial application can't be recorded.
- **Locking**: SQLite `lock/unlock` are no-ops ([sqlite migrate.rs](https://github.com/launchbadge/sqlx/blob/main/sqlx-sqlite/src/migrate.rs)).
  `-- no-transaction` opt-out per file.

### Django
- **Versions**: per-app names `0001_initial`, `0002_…`; numbers are cosmetic, ordering is the
  `dependencies` graph (cross-app deps allowed). Conflicting leaf nodes detected → merge migration
  ([topics/migrations](https://docs.djangoproject.com/en/5.2/topics/migrations/)).
- **State**: `django_migrations(app, name, applied)`; refuses if a migration is applied but its
  dependency isn't (inconsistent history).
- **Down**: auto-reverse for model ops; `RunSQL(sql, reverse_sql)` — no reverse → `IrreversibleError`.
- **Tx**: one per migration on SQLite/Postgres (`atomic=False` opt-out). SQLite schema editor emulates
  ALTER by table rebuild ("slow and occasionally buggy").
- **Squashing**: `replaces=[…]` lets new installs use the squashed file while old DBs continue; delete
  old files only after all deployments passed them.

## 3. Comparison at a glance

| Tool | Version id | Down | State | Checksum | Partial-failure handling |
|---|---|---|---|---|---|
| Alembic | hex DAG | fn | table (heads) | no | tx (where supported) |
| Rails | timestamp, OOO ok | fn / Irreversible | table (set) | no | tx per migration |
| Flyway | V-number, OOO opt | U files (paid) | table | yes | `failed` row + `repair` |
| goose | ts/seq, hybrid | marker in same file | table (log) | no | tx per file |
| golang-migrate | uint, paired files | `.down.sql` | 1 row + `dirty` | no | dirty → manual `force` |
| refinery | V/U int | none | table | yes | tx per migration/grouped |
| rusqlite_migration | Vec index | optional | **user_version** | no | one tx for whole run |
| sqlx | i64, paired | `.down.sql` | table | yes | tx; `success=false` = dirty |
| Django | named DAG | auto/`reverse_sql` | table | no | tx per migration |

## 4. Synthesis for remuda

Design point: daemon is the only process touching the DBs, SQLite only, plain `.sql` up/down files
shipped by mods (`NNNN_name.up.sql` / `NNNN_name.down.sql`, or goose-style markers in one file).

**Practices that fit**
1. **Sequential integers, contiguous, no out-of-order.** Timestamps/DAGs solve multi-developer merge
   races in one app repo; a mod's migrations are authored serially by one owner. Contiguity makes
   `user_version` sufficient and gaps detectable as a load error (golang-migrate/rusqlite_migration
   style). Reject duplicates/gaps at mod load, before touching the DB.
2. **One transaction per run, version written in the same tx** (rusqlite_migration). Use
   `BEGIN IMMEDIATE`; apply N..M; `PRAGMA user_version=M`; COMMIT. Because SQLite DDL is transactional
   and the version lives in the same file header, a crash leaves either old schema+old version or new
   schema+new version — **no dirty flag needed** (golang-migrate's dirty exists only because its
   version write is a separate tx / non-transactional backends).
3. **`user_version` for the version, plus a small meta table for checksums** (Flyway/sqlx/refinery
   drift detection). Checksums of applied up-files (and the stored down SQL, see 6) catch a mod
   author editing a shipped migration. Policy on mismatch: refuse to open (sqlx) or warn — decide.
4. **Refuse newer schema.** If `user_version` > highest migration the mod ships → error
   (`DatabaseTooFarAhead`, sqlx `VersionMissing`). Never "best-effort" open; an old mod writing to a
   new schema corrupts data silently.
5. **Runner owns FK/PRAGMA handling.** Turn `foreign_keys` OFF on the migration connection *before*
   BEGIN, run `PRAGMA foreign_key_check` before COMMIT (abort on rows), restore after. Keep the db_lua
   authorizer denying PRAGMAs inside migration files (a `PRAGMA foreign_keys` there is a silent no-op
   anyway; `user_version` must be runner-only). Allow the 12-step rebuild statements.
6. **Downgrade = explicit, opt-in, all-or-nothing.** Validate the whole down range has `.down.sql`
   before starting (rusqlite_migration `DownNotDefined`). Empty/absent down = irreversible (Rails/
   Django). Since the *newer* code holds the down SQL and an older mod may be the one running, consider
   **storing each applied migration's down SQL in the meta table at apply time** so a rollback can be
   performed by the older binary/mod — none of the surveyed tools do this, and it is the main gap for
   "older version meets newer schema".
7. **Data-loss downgrades**: tools don't detect them — it's the author's job (`DROP COLUMN` in a down
   file loses data). Mitigate with a pre-migration backup (`VACUUM INTO` / backup API, outside tx) and
   make destructive downs explicit (e.g. require a flag).
8. **Locking**: the daemon serialises per DB (one migration task per file) and `BEGIN IMMEDIATE`
   guards against anything else; no advisory lock table (SQLite drivers in sqlx/golang-migrate are
   no-ops / in-process only anyway).
9. **application_id** on daemon-owned DBs (e.g. `vfs/index.db`) to recognise foreign files; keep
   `user_version` semantics as-is for vfs_index_db (already =1) and fold its legacy JSON import in as
   migration 1→ behaviour.
10. **Baseline/squash later**: Django's `replaces` / Rails schema snapshot — optional; only if mod
    migration chains grow long.

**Pitfalls to avoid**
- `PRAGMA foreign_keys=OFF` inside a migration tx (no-op), or leaving FKs off after a failure.
- Renaming the old table first in a rebuild (breaks FK/trigger/view refs); turning on
  `legacy_alter_table`.
- Writing `PRAGMA schema_version` (corruption) or letting mods write `user_version` directly.
- Splitting SQL on `;` naïvely — breaks `CREATE TRIGGER … BEGIN …; END`. Use
  `sqlite3_prepare` tail iteration (rusqlite `execute_batch`) rather than a text splitter.
- Statements that cannot run in a tx (`VACUUM`, `journal_mode`) in migration files — reject them.
- Editing a shipped migration file (checksum drift); treat as error, fix via new migration.
- Idempotent `IF NOT EXISTS` everywhere hides broken downs (golang-migrate warning); with atomic tx
  it isn't needed.
- 0-byte migration files (golang-migrate notes errors); require a comment for intentional no-ops.
- Long rebuilds on large tables hold the write lock for the whole run; acceptable for mod DBs but
  log duration.
