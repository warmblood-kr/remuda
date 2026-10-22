//! `remuda.db`: SQLite databases for mods (#778), on the daemon's Lua thread.
//!
//! Each mod owns a namespace: `remuda.db.open("notes")` in mod `butler`
//! opens `<state>/remuda/db/butler/notes.db`. `tools.lua` passes the running
//! mod's name (its `current_owner`), so a mod names only the database. Files
//! are private through `crate::sqlite::open`.
//!
//! Lua is not a sandbox (`script.rs`), so the limits guard against mistakes
//! that would hang or bloat the daemon, not against an attacker:
//! - size: `PRAGMA max_page_count`, so a write past it fails `too_large`;
//! - time: a progress handler interrupts a statement past `timeout_ms`;
//! - rows: a query returning more than `max_rows` fails `too_many_rows`;
//! - reach: an authorizer refuses ATTACH, DETACH and every PRAGMA outside a
//!   short read-mostly list, so a handle cannot open another file (the VFS
//!   index included) or undo the limits.
//!
//! `opts.migrations` (the mod's `migrations/NAME/` files, or inline steps)
//! runs `sqlite_migrate` before the handle is returned.
//!
//! Failures are `nil, "code: detail"`; Lua usage errors raise.

use crate::cluster::storage;
use crate::sqlite_migrate;
use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, ErrorCode};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Per-handle limits. Defaults, overridable per `open` up to `CEILING`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: u64,
    pub timeout_ms: u64,
    pub max_rows: usize,
}

pub const DEFAULT_LIMITS: Limits = Limits {
    max_bytes: 64 * 1024 * 1024,
    timeout_ms: 1_000,
    max_rows: 10_000,
};

/// The most an `open` may ask for.
pub const CEILING: Limits = Limits {
    max_bytes: 1024 * 1024 * 1024,
    timeout_ms: 10_000,
    max_rows: 100_000,
};

/// PRAGMAs a handle may run. Everything else (journal_mode, max_page_count,
/// writable_schema, ...) could undo privacy or the limits.
const ALLOWED_PRAGMAS: [&str; 12] = [
    "data_version",
    "foreign_key_check",
    "foreign_key_list",
    "freelist_count",
    "index_info",
    "index_list",
    "index_xinfo",
    "integrity_check",
    "page_count",
    "quick_check",
    "table_info",
    "user_version",
];

/// Bind `remuda._db_open(owner, name, opts)`; `tools.lua` wraps it.
pub(crate) fn register(lua: &Lua, remuda: &Table) -> mlua::Result<()> {
    remuda.set(
        "_db_open",
        lua.create_function(
            |lua, (owner, name, opts): (String, String, Option<Table>)| {
                let limits = limits_from(opts.as_ref())?;
                let migrations = migrations_from(opts.as_ref())?;
                let root = match db_root() {
                    Ok(root) => root,
                    Err(error) => return failure(lua, format!("io: {error}")),
                };
                open_answer(lua, &root, &owner, &name, limits, migrations)
            },
        )?,
    )
}

pub(crate) fn db_root() -> std::io::Result<PathBuf> {
    let root = storage::cluster_state_dir()?.join("db");
    storage::private_directory(&root)?;
    Ok(root)
}

fn open_answer(
    lua: &Lua,
    root: &Path,
    owner: &str,
    name: &str,
    limits: Limits,
    migrations: Migrations,
) -> mlua::Result<(Value, Value)> {
    check_identifier("mod", owner, true)?;
    check_identifier("database", name, false)?;
    let steps = match migrations {
        Migrations::None => sqlite_migrate::StepSet::plain(Vec::new()),
        Migrations::Steps(steps) => sqlite_migrate::StepSet::plain(steps),
        Migrations::Files => match migration_files(owner, name) {
            Ok(steps) => steps,
            Err(reason) => return failure(lua, reason),
        },
    };
    match open_db(&root.join(owner), name, limits, &steps) {
        Ok(db) => Ok((Value::UserData(lua.create_userdata(db)?), Value::Nil)),
        Err(reason) => failure(lua, reason),
    }
}

fn failure(lua: &Lua, reason: String) -> mlua::Result<(Value, Value)> {
    Ok((Value::Nil, Value::String(lua.create_string(reason)?)))
}

/// A plain identifier, never a path. A mod name may also hold `-`.
fn check_identifier(what: &str, text: &str, dash: bool) -> mlua::Result<()> {
    let mut bytes = text.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
    let rest_ok = bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || (dash && b == b'-'));
    if first_ok && rest_ok && text.len() <= 64 {
        Ok(())
    } else {
        Err(mlua::Error::runtime(format!(
            "remuda.db: {what} name {text:?} must be a plain identifier (letters, digits, _), at most 64 characters"
        )))
    }
}

/// What `opts.migrations` asked for.
pub enum Migrations {
    None,
    Files,
    Steps(Vec<sqlite_migrate::Step>),
}

fn migrations_from(opts: Option<&Table>) -> mlua::Result<Migrations> {
    let Some(opts) = opts else {
        return Ok(Migrations::None);
    };
    let usage = || {
        mlua::Error::runtime(
            "remuda.db.open: migrations must be true or an array of { name = ..., up = ..., down = ... }",
        )
    };
    match opts.get::<Value>("migrations")? {
        Value::Nil | Value::Boolean(false) => Ok(Migrations::None),
        Value::Boolean(true) => Ok(Migrations::Files),
        Value::Table(list) => {
            let mut steps = Vec::new();
            for index in 1..=list.raw_len() {
                let Value::Table(entry) = list.raw_get::<Value>(index)? else {
                    return Err(usage());
                };
                steps.push(sqlite_migrate::Step {
                    name: entry.get::<Option<String>>("name")?.ok_or_else(usage)?,
                    up: entry.get::<Option<String>>("up")?.ok_or_else(usage)?,
                    down: entry.get("down")?,
                });
            }
            Ok(Migrations::Steps(steps))
        }
        _ => Err(usage()),
    }
}

/// The installed mod's `migrations/NAME/` files.
fn migration_files(owner: &str, name: &str) -> Result<sqlite_migrate::StepSet, String> {
    let dir = crate::packages::package_dir(owner)
        .map_err(|error| format!("migration: {error}"))?
        .ok_or_else(|| {
            format!("migration: mod {owner} is not installed, so it has no migration files")
        })?
        .join("migrations")
        .join(name);
    sqlite_migrate::load_dir(&dir)
}

fn limits_from(opts: Option<&Table>) -> mlua::Result<Limits> {
    let mut limits = DEFAULT_LIMITS;
    let Some(opts) = opts else {
        return Ok(limits);
    };
    let field = |key: &str, ceiling: u64| -> mlua::Result<Option<u64>> {
        match opts.get::<Option<u64>>(key)? {
            Some(value) if value == 0 || value > ceiling => Err(mlua::Error::runtime(format!(
                "remuda.db.open: {key} must be 1..{ceiling}"
            ))),
            other => Ok(other),
        }
    };
    if let Some(value) = field("max_bytes", CEILING.max_bytes)? {
        limits.max_bytes = value;
    }
    if let Some(value) = field("timeout_ms", CEILING.timeout_ms)? {
        limits.timeout_ms = value;
    }
    if let Some(value) = field("max_rows", CEILING.max_rows as u64)? {
        limits.max_rows = value as usize;
    }
    Ok(limits)
}

/// An open mod database. `close` drops the connection; later calls fail.
pub struct Db {
    conn: RefCell<Option<Connection>>,
    deadline: Arc<Mutex<Option<Instant>>>,
    limits: Limits,
}

fn open_db(
    dir: &Path,
    name: &str,
    limits: Limits,
    steps: &sqlite_migrate::StepSet,
) -> Result<Db, String> {
    let file = format!("{name}.db");
    let mut conn = crate::sqlite::open(dir, &file).map_err(|e| format!("io: {e}"))?;
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(reason)?;
    let pages = (limits.max_bytes / page_size.max(1) as u64).max(1);
    let _: i64 = conn
        .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |row| {
            row.get(0)
        })
        .map_err(reason)?;
    let deadline = Arc::new(Mutex::new(None::<Instant>));
    let watched = Arc::clone(&deadline);
    conn.progress_handler(
        1_000,
        Some(move || {
            watched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some_and(|deadline| Instant::now() >= deadline)
        }),
    )
    .map_err(reason)?;
    // Migrations get the ceiling's time; the size limit is the handle's.
    *deadline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(Instant::now() + Duration::from_millis(CEILING.timeout_ms));
    let migrated = sqlite_migrate::upgrade_set(&mut conn, &file, steps, false);
    *deadline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    migrated.map_err(|error| migration_reason(error, limits))?;
    conn.authorizer(Some(authorize)).map_err(reason)?;
    Ok(Db {
        conn: RefCell::new(Some(conn)),
        deadline,
        limits,
    })
}

fn migration_reason(error: String, limits: Limits) -> String {
    if error.contains("interrupted") {
        format!(
            "timeout: migrations ran longer than {} ms",
            CEILING.timeout_ms
        )
    } else if error.contains("database or disk is full") {
        format!(
            "too_large: migrations reached the limit of {} bytes",
            limits.max_bytes
        )
    } else {
        error
    }
}

/// The handle's reach. `user_version` and `_remuda_*` tables belong to the
/// migration runner, so a handle may read them but not write them.
fn authorize(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => {
            let name = pragma_name.to_ascii_lowercase();
            let writes_version = name == "user_version" && pragma_value.is_some();
            if ALLOWED_PRAGMAS.contains(&name.as_str()) && !writes_version {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        action => sqlite_migrate::deny_internal_write(action),
    }
}

impl Db {
    /// Run `work` on the live connection with the statement deadline set.
    fn with_conn<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let guard = self.conn.borrow();
        let conn = guard
            .as_ref()
            .ok_or_else(|| "closed: this database handle was closed".to_owned())?;
        let limit = Duration::from_millis(self.limits.timeout_ms);
        self.set_deadline(Some(Instant::now() + limit));
        let result = work(conn);
        self.set_deadline(None);
        result.map_err(|error| self.explain(error))
    }

    fn set_deadline(&self, value: Option<Instant>) {
        *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
    }

    /// Name the limit behind a failure, so a caller can tell them apart.
    fn explain(&self, error: String) -> String {
        if error.starts_with("interrupted") {
            format!(
                "timeout: statement ran longer than {} ms",
                self.limits.timeout_ms
            )
        } else if error.starts_with("full") {
            format!(
                "too_large: database reached its limit of {} bytes",
                self.limits.max_bytes
            )
        } else {
            error
        }
    }

    fn query(&self, lua: &Lua, sql: &str, params: Vec<SqlValue>) -> Result<Table, String> {
        let max_rows = self.limits.max_rows;
        let rows = self.with_conn(|conn| {
            let mut statement = conn.prepare(sql).map_err(reason)?;
            let names: Vec<String> = statement
                .column_names()
                .iter()
                .map(|n| (*n).to_owned())
                .collect();
            let mut cursor = statement
                .query(rusqlite::params_from_iter(params))
                .map_err(reason)?;
            let mut out = Vec::new();
            while let Some(row) = cursor.next().map_err(reason)? {
                if out.len() == max_rows {
                    return Err(format!(
                        "too_many_rows: the query returned more than {max_rows} rows"
                    ));
                }
                let mut values = Vec::with_capacity(names.len());
                for index in 0..names.len() {
                    values.push(row.get::<_, SqlValue>(index).map_err(reason)?);
                }
                out.push(values);
            }
            Ok((names, out))
        })?;
        rows_table(lua, rows).map_err(|error| format!("lua: {error}"))
    }

    fn exec(&self, sql: &str, params: Vec<SqlValue>) -> Result<usize, String> {
        self.with_conn(|conn| {
            conn.execute(sql, rusqlite::params_from_iter(params))
                .map_err(reason)
        })
    }

    fn batch(&self, sql: &str) -> Result<(), String> {
        self.with_conn(|conn| conn.execute_batch(sql).map_err(reason))
    }
}

fn rows_table(lua: &Lua, (names, rows): (Vec<String>, Vec<Vec<SqlValue>>)) -> mlua::Result<Table> {
    let out = lua.create_table_with_capacity(rows.len(), 0)?;
    for (position, values) in rows.into_iter().enumerate() {
        let row = lua.create_table_with_capacity(0, names.len())?;
        for (name, value) in names.iter().zip(values) {
            row.set(name.as_str(), to_lua(lua, value)?)?;
        }
        out.set(position + 1, row)?;
    }
    Ok(out)
}

fn to_lua(lua: &Lua, value: SqlValue) -> mlua::Result<Value> {
    Ok(match value {
        SqlValue::Null => Value::Nil,
        SqlValue::Integer(number) => Value::Integer(number),
        SqlValue::Real(number) => Value::Number(number),
        SqlValue::Text(text) => Value::String(lua.create_string(text)?),
        SqlValue::Blob(bytes) => Value::String(lua.create_string(bytes)?),
    })
}

/// Lua parameters: nil, or an array of nil/boolean/number/string values.
fn from_lua(params: Option<Table>) -> mlua::Result<Vec<SqlValue>> {
    let Some(params) = params else {
        return Ok(Vec::new());
    };
    let count = params.raw_len();
    let mut out = Vec::with_capacity(count);
    for index in 1..=count {
        out.push(match params.raw_get::<Value>(index)? {
            Value::Nil => SqlValue::Null,
            Value::Boolean(flag) => SqlValue::Integer(i64::from(flag)),
            Value::Integer(number) => SqlValue::Integer(number),
            Value::Number(number) => SqlValue::Real(number),
            Value::String(text) => match text.to_str() {
                Ok(text) => SqlValue::Text(text.to_owned()),
                Err(_) => SqlValue::Blob(text.as_bytes().to_vec()),
            },
            other => {
                return Err(mlua::Error::runtime(format!(
                    "remuda.db: parameter {index} is a {}, not nil, boolean, number or string",
                    other.type_name()
                )))
            }
        });
    }
    Ok(out)
}

/// `code: detail` for a SQLite failure; `interrupted`/`full` are renamed by
/// `Db::explain` once the handle's limits are known.
fn reason(error: rusqlite::Error) -> String {
    match error.sqlite_error_code() {
        Some(ErrorCode::OperationInterrupted) => "interrupted".into(),
        Some(ErrorCode::DiskFull) => "full".into(),
        Some(ErrorCode::AuthorizationForStatementDenied) => {
            format!("denied: {error} (ATTACH, DETACH and most PRAGMAs are not allowed)")
        }
        _ => format!("sql: {error}"),
    }
}

fn answer<T: mlua::IntoLua>(lua: &Lua, result: Result<T, String>) -> mlua::Result<(Value, Value)> {
    match result {
        Ok(value) => Ok((value.into_lua(lua)?, Value::Nil)),
        Err(reason) => failure(lua, reason),
    }
}

impl UserData for Db {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "query",
            |lua, this, (sql, params): (String, Option<Table>)| {
                let params = from_lua(params)?;
                let result = this.query(lua, &sql, params);
                answer(lua, result)
            },
        );
        methods.add_method(
            "exec",
            |lua, this, (sql, params): (String, Option<Table>)| {
                let params = from_lua(params)?;
                answer(lua, this.exec(&sql, params))
            },
        );
        methods.add_method("batch", |lua, this, sql: String| {
            answer(lua, this.batch(&sql).map(|()| true))
        });
        methods.add_method("close", |_, this, ()| {
            drop(this.conn.borrow_mut().take());
            Ok(true)
        });
        methods.add_method("limits", |lua, this, ()| {
            let table = lua.create_table()?;
            table.set("max_bytes", this.limits.max_bytes)?;
            table.set("timeout_ms", this.limits.timeout_ms)?;
            table.set("max_rows", this.limits.max_rows)?;
            Ok(table)
        });
    }
}

#[cfg(test)]
mod tests;
