//! Schema migrations for daemon-owned SQLite databases (#778).
//!
//! Steps are numbered 1..N with plain SQL: an `up` and an optional `down`.
//! `PRAGMA user_version` is the applied version. `_remuda_migrations` keeps one
//! row per applied step: its name, the sha256 of its `up`, and its `down`, so
//! any build with this runner can step back without the newer code.
//!
//! A run is one `BEGIN IMMEDIATE`: every pending step, the rows and
//! `user_version` commit together, so a crash leaves the old schema or the
//! new one. Foreign keys are off during a run and checked before commit.
//! Step SQL runs under an authorizer: no ATTACH, no transaction control, no
//! PRAGMA but `foreign_key_check`, and no write to `_remuda_*` tables.
//! A base (`NNNN_slug.base.sql`) creates a new database at step NNNN, so a mod
//! can delete older step files; its record then starts at NNNN.
//!
//! See docs/design-778-sqlite-migrations.md.

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// The table that records applied steps. Mod code may not write it.
pub const TABLE: &str = "_remuda_migrations";

/// One numbered step. `down: None` makes the step irreversible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    pub up: String,
    pub down: Option<String>,
}

/// `NNNN_slug.base.sql`: the whole schema as of step `version`. A new
/// database runs it instead of steps 1..=version, so a mod can delete them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    pub version: u32,
    pub name: String,
    pub up: String,
}

/// What a mod ships: an optional base, then steps numbered from `first`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepSet {
    pub base: Option<Base>,
    pub first: u32,
    pub steps: Vec<Step>,
}

impl StepSet {
    pub fn plain(steps: Vec<Step>) -> Self {
        Self {
            base: None,
            first: 1,
            steps,
        }
    }

    pub fn target(&self) -> u32 {
        match (self.steps.len(), &self.base) {
            (0, Some(base)) => base.version,
            (count, _) => self.first + count as u32 - 1,
        }
    }

    pub fn step(&self, version: u32) -> Option<&Step> {
        let index = version.checked_sub(self.first)?;
        self.steps.get(index as usize)
    }
}

/// A recorded step, as `status` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub version: u32,
    pub name: String,
    pub up_sha256: String,
    pub reversible: bool,
}

/// What `upgrade` did: the version before and after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moved {
    pub from: u32,
    pub to: u32,
}

/// Load `NNNN_slug.up.sql` (and optional `.down.sql`) files and at most one
/// `NNNN_slug.base.sql` from `dir`. Up files run without gaps, from 1 or
/// from at most the base's number + 1; an empty file is an error.
pub fn load_dir(dir: &Path) -> Result<StepSet, String> {
    let entries = fs::read_dir(dir)
        .map_err(|error| format!("migration: cannot read {}: {error}", dir.display()))?;
    let mut ups = std::collections::BTreeMap::new();
    let mut downs = std::collections::BTreeMap::new();
    let mut base: Option<Base> = None;
    for entry in entries {
        let entry = entry.map_err(|error| format!("migration: {}: {error}", dir.display()))?;
        let file = entry.file_name().to_string_lossy().into_owned();
        let (stem, kind) = if let Some(stem) = file.strip_suffix(".up.sql") {
            (stem, Kind::Up)
        } else if let Some(stem) = file.strip_suffix(".down.sql") {
            (stem, Kind::Down)
        } else if let Some(stem) = file.strip_suffix(".base.sql") {
            (stem, Kind::Base)
        } else {
            return Err(format!(
                "migration: {file} is not NNNN_name.up.sql, .down.sql or .base.sql"
            ));
        };
        let (number, name) = parse_stem(stem)
            .ok_or_else(|| format!("migration: {file} does not start with a number and _"))?;
        let sql = fs::read_to_string(entry.path())
            .map_err(|error| format!("migration: cannot read {file}: {error}"))?;
        let target = match kind {
            Kind::Up => &mut ups,
            Kind::Down => &mut downs,
            Kind::Base => {
                if number < 2 || base.is_some() {
                    return Err(format!(
                        "migration: {file}: a mod has at most one base, numbered 2 or more"
                    ));
                }
                if sql.trim().is_empty() {
                    return Err(format!("migration: {file} is empty"));
                }
                let name = name.to_owned();
                base = Some(Base {
                    version: number,
                    name,
                    up: sql,
                });
                continue;
            }
        };
        if target.insert(number, (name.to_owned(), sql)).is_some() {
            return Err(format!("migration: step {number} appears twice"));
        }
    }
    let first = ups.keys().next().copied().unwrap_or(1);
    let base_version = base.as_ref().map_or(0, |base| base.version);
    if first > base_version + 1 {
        return Err(format!("migration: step {} is missing", base_version + 1));
    }
    if let Some(&last) = ups.keys().next_back() {
        if last < base_version {
            return Err(format!(
                "migration: step {last} is before the base ({base_version})"
            ));
        }
    }
    let mut steps = Vec::with_capacity(ups.len());
    for (number, (name, up)) in ups {
        let down = match downs.remove(&number) {
            Some((down_name, _)) if down_name != name => {
                return Err(format!(
                    "migration: step {number} is {name} up but {down_name} down"
                ))
            }
            Some((_, sql)) => Some(sql),
            None => None,
        };
        let expected = first + steps.len() as u32;
        if number != expected {
            return Err(format!("migration: step {expected} is missing"));
        }
        steps.push(Step { name, up, down });
    }
    if let Some(number) = downs.keys().next() {
        return Err(format!(
            "migration: step {number} has a down file but no up file"
        ));
    }
    check_from(first, &steps)?;
    Ok(StepSet { base, first, steps })
}

enum Kind {
    Up,
    Down,
    Base,
}

fn parse_stem(stem: &str) -> Option<(u32, &str)> {
    let (digits, name) = stem.split_once('_')?;
    let ok = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    let named = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    (ok && named).then(|| digits.parse().ok().map(|n| (n, name)))?
}

/// Reject empty SQL and empty names. An intended no-op holds a comment.
fn check_from(first: u32, steps: &[Step]) -> Result<(), String> {
    for (index, step) in steps.iter().enumerate() {
        let number = first as usize + index;
        if step.name.is_empty() {
            return Err(format!("migration: step {number} has no name"));
        }
        if step.up.trim().is_empty() || step.down.as_deref().is_some_and(|d| d.trim().is_empty()) {
            return Err(format!(
                "migration: step {number} ({}) has an empty file",
                step.name
            ));
        }
    }
    Ok(())
}

pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn version(conn: &Connection) -> Result<u32, String> {
    let value: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(sql)?;
    u32::try_from(value)
        .map_err(|_| format!("migration: user_version {value} is not a step number"))
}

fn has_table(conn: &Connection) -> Result<bool, String> {
    conn.query_row(
        "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [TABLE],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
    .map_err(sql)
}

/// Every recorded step, oldest first.
pub fn status(conn: &Connection) -> Result<Vec<Applied>, String> {
    if !has_table(conn)? {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare(&format!(
            "SELECT version, name, up_sha256, down_sql IS NOT NULL FROM {TABLE} ORDER BY version"
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok(Applied {
                version: row.get(0)?,
                name: row.get(1)?,
                up_sha256: row.get(2)?,
                reversible: row.get(3)?,
            })
        })
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// Bring `conn` to `steps.len()`. Refuses a newer database (`too_new:`) and an
/// applied step whose `up` changed (`drift:`). `adopt` records steps 1..v of an
/// unrecorded `user_version` v instead of refusing it (the VFS index).
pub fn upgrade(
    conn: &mut Connection,
    label: &str,
    steps: &[Step],
    adopt: bool,
) -> Result<Moved, String> {
    u32::try_from(steps.len()).map_err(|_| "migration: too many steps".to_owned())?;
    upgrade_set(conn, label, &StepSet::plain(steps.to_vec()), adopt)
}

/// `upgrade` for a set that may start with a base: a new database runs the
/// base, records it at its number, then runs the later steps.
pub fn upgrade_set(
    conn: &mut Connection,
    label: &str,
    set: &StepSet,
    adopt: bool,
) -> Result<Moved, String> {
    check_from(set.first, &set.steps)?;
    let target = set.target();
    let (from, adopting) = plan(conn, label, set, adopt)?;
    if from == target && !adopting {
        return Ok(Moved { from, to: target });
    }
    // Plan again under the write lock: another connection may have run the
    // steps between our read and the lock.
    let mut moved = Moved { from, to: target };
    in_run(conn, |tx| {
        let (from, adopting) = plan(tx, label, set, adopt)?;
        moved.from = from;
        if from == target && !adopting {
            return Ok(());
        }
        create_table(tx)?;
        let mut at = from;
        if adopting {
            for number in 1..=from {
                record(tx, number, wanted(set, label, number)?)?;
            }
        } else if let (0, Some(base)) = (from, &set.base) {
            run_step_sql(tx, &base.up).map_err(|error| {
                format!("{error} (in {label} base {}, {})", base.version, base.name)
            })?;
            let step = Step {
                name: base.name.clone(),
                up: base.up.clone(),
                down: None,
            };
            record(tx, base.version, &step)?;
            at = base.version;
        }
        for number in at + 1..=target {
            let step = wanted(set, label, number)?;
            run_step_sql(tx, &step.up)
                .map_err(|error| format!("{error} (in {label} step {number}, {})", step.name))?;
            record(tx, number, step)?;
        }
        set_version(tx, target)
    })?;
    Ok(moved)
}

fn wanted<'a>(set: &'a StepSet, label: &str, number: u32) -> Result<&'a Step, String> {
    set.step(number)
        .ok_or_else(|| squashed(label, number - 1, set))
}

fn squashed(label: &str, from: u32, set: &StepSet) -> String {
    let base = set.base.as_ref().map_or(String::new(), |base| {
        format!(" into {:04}_{}.base.sql", base.version, base.name)
    });
    format!(
        "squashed: {label} is at version {from}, but steps 1-{} were squashed{base}; open it once with an older release of the mod",
        set.first - 1
    )
}

fn record(tx: &Connection, number: u32, step: &Step) -> Result<(), String> {
    tx.execute(
        &format!(
            "INSERT INTO {TABLE} (version, name, up_sha256, down_sql, applied_at) VALUES (?1, ?2, ?3, ?4, ?5)"
        ),
        params![number, step.name, sha256_hex(&step.up), step.down, unix_now()],
    )
    .map(|_| ())
    .map_err(sql)
}

/// Check the applied steps against `set`: the applied version, and whether
/// an unrecorded `user_version` is adopted. Refuses too new, unrecorded,
/// drifted, or below a squash.
fn plan(conn: &Connection, label: &str, set: &StepSet, adopt: bool) -> Result<(u32, bool), String> {
    let target = set.target();
    let from = version(conn)?;
    let recorded = status(conn)?;
    if from > target {
        return Err(format!(
            "too_new: {label} is at version {from}, this code knows {target}; upgrade it, or step the database back with `remuda db downgrade`"
        ));
    }
    let adopting = adopt && recorded.is_empty() && from > 0;
    if !adopting {
        check_record(label, &recorded, from)?;
    }
    if let Some(applied) = drifted(set, &recorded) {
        return Err(format!(
            "drift: {label} step {} ({}) changed after it was applied; add a new step instead of editing it",
            applied.version, applied.name
        ));
    }
    if from > 0 && from + 1 < set.first {
        return Err(squashed(label, from, set));
    }
    Ok((from, adopting))
}

/// The record runs without gaps up to `from`, from 1 or, for a database
/// created from a base, from the base's number.
fn check_record(label: &str, recorded: &[Applied], from: u32) -> Result<(), String> {
    let first = recorded.first().map_or(from + 1, |applied| applied.version);
    if from + 1 - first != recorded.len() as u32 || (recorded.is_empty() && from > 0) {
        return Err(format!(
            "migration: {label} has user_version {from} but {} recorded steps; it was changed outside the migration runner",
            recorded.len()
        ));
    }
    for (index, applied) in recorded.iter().enumerate() {
        if applied.version != first + index as u32 {
            return Err(format!(
                "migration: {label} is missing the record of step {}",
                first + index as u32
            ));
        }
    }
    Ok(())
}

/// The first recorded step whose shipped file differs. Steps no longer
/// shipped (squashed) are not checked; a base row is checked against a base.
pub fn drifted<'a>(set: &StepSet, recorded: &'a [Applied]) -> Option<&'a Applied> {
    let base_row = recorded
        .first()
        .map(|applied| applied.version)
        .filter(|&v| v > 1);
    recorded.iter().find(|applied| {
        let shipped = if base_row == Some(applied.version) {
            set.base
                .as_ref()
                .filter(|base| base.version == applied.version)
                .map(|base| (&base.name, &base.up))
        } else {
            set.step(applied.version).map(|step| (&step.name, &step.up))
        };
        shipped
            .is_some_and(|(name, up)| *name != applied.name || sha256_hex(up) != applied.up_sha256)
    })
}

/// Step back to `to` with the stored `down` SQL, newest first, in one run.
/// Refused before anything runs if a step in the range is irreversible.
pub fn downgrade(conn: &mut Connection, label: &str, to: u32) -> Result<Moved, String> {
    let mut moved = Moved { from: 0, to };
    // Checked under the write lock, so a concurrent run cannot slip in.
    in_run(conn, |tx| {
        let from = version(tx)?;
        let recorded = status(tx)?;
        check_record(label, &recorded, from)?;
        if to >= from {
            return Err(format!(
                "migration: {label} is at version {from}; nothing to undo down to {to}"
            ));
        }
        let downs = planned_downs(tx, to)?;
        if let Some((number, name, _)) = downs.iter().find(|(_, _, down)| down.is_none()) {
            return Err(format!(
                "irreversible: {label} step {number} ({name}) has no down SQL; nothing was changed"
            ));
        }
        for (number, name, down) in &downs {
            let down = down.as_deref().unwrap_or_default();
            run_step_sql(tx, down)
                .map_err(|error| format!("{error} (in {label} step {number} down, {name})"))?;
            tx.execute(&format!("DELETE FROM {TABLE} WHERE version = ?1"), [number])
                .map_err(sql)?;
        }
        moved.from = from;
        set_version(tx, to)
    })?;
    Ok(moved)
}

/// The stored downs above `to`, newest first: (version, name, down SQL).
pub fn planned_downs(
    conn: &Connection,
    to: u32,
) -> Result<Vec<(u32, String, Option<String>)>, String> {
    if !has_table(conn)? {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare(&format!(
            "SELECT version, name, down_sql FROM {TABLE} WHERE version > ?1 ORDER BY version DESC"
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map([to], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// Copy the database to a new private file next to it (`VACUUM INTO`).
pub fn backup(conn: &Connection, db_path: &Path, version: u32) -> Result<PathBuf, String> {
    let name = db_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = db_path.with_file_name(format!("{name}.v{version}-{}.bak", unix_now()));
    if target.exists() {
        return Err(format!("io: backup {} already exists", target.display()));
    }
    crate::cluster::storage::private_file(&target).map_err(|error| format!("io: {error}"))?;
    conn.execute("VACUUM INTO ?1", [target.to_string_lossy()])
        .map_err(sql)?;
    Ok(target)
}

fn create_table(tx: &Connection) -> Result<(), String> {
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {TABLE} (
             version    INTEGER PRIMARY KEY NOT NULL,
             name       TEXT NOT NULL,
             up_sha256  TEXT NOT NULL,
             down_sql   TEXT,
             applied_at INTEGER NOT NULL
         );"
    ))
    .map_err(sql)
}

fn set_version(tx: &Connection, version: u32) -> Result<(), String> {
    tx.pragma_update(None, "user_version", version).map_err(sql)
}

/// Foreign keys off, `BEGIN IMMEDIATE`, `work`, `foreign_key_check`, commit;
/// foreign keys go back to how they were whatever happens.
fn in_run(
    conn: &mut Connection,
    work: impl FnOnce(&Connection) -> Result<(), String>,
) -> Result<(), String> {
    let keys: bool = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(sql)?;
    conn.pragma_update(None, "foreign_keys", false)
        .map_err(sql)?;
    let result = (|| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        work(&tx)?;
        let violation: Option<String> = tx
            .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
            .optional()
            .map_err(sql)?;
        if let Some(table) = violation {
            return Err(format!(
                "foreign_key: a step left rows in {table} that break a foreign key"
            ));
        }
        tx.commit().map_err(sql)
    })();
    let restored = conn.pragma_update(None, "foreign_keys", keys).map_err(sql);
    result.and(restored)
}

/// Run one step's SQL under `step_authorize`, then drop the authorizer.
fn run_step_sql(conn: &Connection, text: &str) -> Result<(), String> {
    conn.authorizer(Some(step_authorize)).map_err(sql)?;
    let result = conn.execute_batch(text).map_err(sql);
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .map_err(sql)?;
    result
}

fn step_authorize(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Attach { .. }
        | AuthAction::Detach { .. }
        | AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. } => Authorization::Deny,
        AuthAction::Pragma { pragma_name, .. } => {
            if pragma_name.eq_ignore_ascii_case("foreign_key_check") {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        action => deny_internal_write(action),
    }
}

/// Deny any write to a `_remuda_*` table; allow everything else.
pub fn deny_internal_write(action: AuthAction<'_>) -> Authorization {
    let table = match action {
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::CreateTable { table_name }
        | AuthAction::DropTable { table_name }
        | AuthAction::AlterTable { table_name, .. }
        | AuthAction::CreateIndex { table_name, .. }
        | AuthAction::DropIndex { table_name, .. }
        | AuthAction::CreateTrigger { table_name, .. }
        | AuthAction::DropTrigger { table_name, .. } => table_name,
        _ => return Authorization::Allow,
    };
    if table.to_ascii_lowercase().starts_with("_remuda_") {
        Authorization::Deny
    } else {
        Authorization::Allow
    }
}

fn sql(error: rusqlite::Error) -> String {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::AuthorizationForStatementDenied) => format!(
            "denied: {error} (steps may not ATTACH, BEGIN/COMMIT, run PRAGMAs or write _remuda_* tables)"
        ),
        _ => format!("sql: {error}"),
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests;
