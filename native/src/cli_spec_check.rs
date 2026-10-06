//! Strict validation of opt-in v2 Lua specs, before any clap command is built.
//! All reads are raw (no `__index`/`__pairs`) and messages never echo values.

use crate::cli_parse::{FEATURES, REPORT_VERSIONS, SPEC_VERSIONS};
use mlua::{Table, Value};
use std::collections::BTreeMap;

/// `("spec" | "unsupported", safe text)` for a rejected spec.
pub type Rejection = (&'static str, String);

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    Str,
    Bool,
    Table,
}

const UPGRADE: &str = "Next: run `remuda upgrade` to install the latest version, then retry.";

fn spec_err(path: &str, reason: &str) -> Rejection {
    (
        "spec",
        format!("remuda: invalid spec: {path}: {reason}\nNext: fix that spec field, then retry."),
    )
}

fn unsupported(what: &str) -> Rejection {
    (
        "unsupported",
        format!("remuda: unsupported: {what}; this core is too old for the spec.\n{UPGRADE}"),
    )
}

/// Field names are echoed only when they look like short identifiers.
fn label(key: &str) -> String {
    let ok = key.len() <= 32
        && !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        key.to_owned()
    } else {
        "<name>".to_owned()
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        label(key)
    } else {
        format!("{path}.{}", label(key))
    }
}

fn fields(t: &Table, path: &str, allowed: &[&str]) -> Result<BTreeMap<String, Value>, Rejection> {
    let mut out = BTreeMap::new();
    for pair in t.pairs::<Value, Value>() {
        let Ok((Value::String(key), value)) = pair else {
            return Err(spec_err(&shown(path), "field names must be strings"));
        };
        let key = key.to_string_lossy();
        if !allowed.contains(&key.as_str()) {
            return Err(spec_err(&join(path, &key), "unknown field"));
        }
        out.insert(key, value);
    }
    Ok(out)
}

fn shown(path: &str) -> String {
    if path.is_empty() {
        "spec".to_owned()
    } else {
        path.to_owned()
    }
}

fn typed<'a>(
    m: &'a BTreeMap<String, Value>,
    key: &str,
    path: &str,
    ty: Ty,
    required: bool,
) -> Result<Option<&'a Value>, Rejection> {
    let at = join(path, key);
    match (m.get(key), ty) {
        (None, _) if required => Err(spec_err(&at, "is required")),
        (None, _) => Ok(None),
        (Some(v @ Value::String(_)), Ty::Str)
        | (Some(v @ Value::Boolean(_)), Ty::Bool)
        | (Some(v @ Value::Table(_)), Ty::Table) => Ok(Some(v)),
        _ => Err(spec_err(&at, "has the wrong type")),
    }
}

fn array(t: &Table, path: &str) -> Result<Vec<Value>, Rejection> {
    let mut items = BTreeMap::new();
    for pair in t.pairs::<Value, Value>() {
        match pair {
            Ok((Value::Integer(i), v)) if i >= 1 && i <= i64::from(u32::MAX) => items.insert(i, v),
            _ => return Err(spec_err(path, "must be a dense array")),
        };
    }
    if items.keys().copied().ne(1..=items.len() as i64) {
        return Err(spec_err(path, "must be a dense array"));
    }
    Ok(items.into_values().collect())
}

fn tables(t: &Table, path: &str) -> Result<Vec<(String, Table)>, Rejection> {
    array(t, path)?
        .into_iter()
        .enumerate()
        .map(|(i, v)| match v {
            Value::Table(t) => Ok((format!("{path}[{}]", i + 1), t)),
            _ => Err(spec_err(&format!("{path}[{}]", i + 1), "must be a table")),
        })
        .collect()
}

/// Version gate: `Ok(true)` means the spec opted into strict v2 validation.
fn gate(t: &Table) -> Result<bool, Rejection> {
    let version = t
        .raw_get::<Value>("version")
        .map_err(|_| spec_err("version", "unreadable"))?;
    let report = t
        .raw_get::<Value>("report_version")
        .map_err(|_| spec_err("report_version", "unreadable"))?;
    let strict = matches!(version, Value::Integer(2)) || matches!(report, Value::Integer(2));
    match version {
        Value::Nil => {}
        Value::Integer(n) if SPEC_VERSIONS.contains(&(n as u32)) && n > 0 && !strict => {}
        Value::Integer(2) => {}
        Value::Integer(n) if n > 2 => {
            return Err(unsupported("spec version is newer than this core supports"))
        }
        Value::Integer(_) if strict => {
            return Err(spec_err("version", "conflicts with report_version = 2"))
        }
        _ => return Err(spec_err("version", "must be an integer 1 or 2")),
    }
    if !strict {
        return Ok(false);
    }
    match report {
        Value::Nil => {}
        Value::Integer(n) if (1..=2).contains(&n) && REPORT_VERSIONS.contains(&(n as u32)) => {}
        Value::Integer(n) if n > 2 => {
            return Err(unsupported(
                "report version is newer than this core supports",
            ))
        }
        _ => return Err(spec_err("report_version", "must be an integer 1 or 2")),
    }
    Ok(true)
}

fn options(v: Option<&Value>, path: &str) -> Result<(), Rejection> {
    let Some(Value::Table(t)) = v else {
        return Ok(());
    };
    for (at, item) in tables(t, path)? {
        let m = fields(&item, &at, &["long", "short", "value", "help", "global"])?;
        for (key, ty, required) in [
            ("long", Ty::Str, true),
            ("short", Ty::Str, false),
            ("value", Ty::Str, false),
            ("help", Ty::Str, true),
            ("global", Ty::Bool, false),
        ] {
            typed(&m, key, &at, ty, required)?;
        }
    }
    Ok(())
}

fn verb(t: &Table, path: &str) -> Result<(), Rejection> {
    let m = fields(t, path, &["about", "next", "args", "options"])?;
    typed(&m, "about", path, Ty::Str, false)?;
    typed(&m, "next", path, Ty::Str, true)?;
    if let Some(Value::Table(args)) = typed(&m, "args", path, Ty::Table, false)? {
        let args_path = join(path, "args");
        for (at, item) in tables(args, &args_path)? {
            let a = fields(&item, &at, &["name", "help", "multiple", "required"])?;
            for (key, ty, required) in [
                ("name", Ty::Str, true),
                ("help", Ty::Str, true),
                ("multiple", Ty::Bool, false),
                ("required", Ty::Bool, false),
            ] {
                typed(&a, key, &at, ty, required)?;
            }
        }
    }
    options(
        typed(&m, "options", path, Ty::Table, false)?,
        &join(path, "options"),
    )
}

/// Check a spec table. `Ok(false)` is a legacy spec: nothing was validated.
pub fn check(t: &Table) -> Result<bool, Rejection> {
    if !gate(t)? {
        return Ok(false);
    }
    let m = fields(
        t,
        "",
        &[
            "version",
            "report_version",
            "requires",
            "name",
            "options",
            "verbs",
        ],
    )?;
    if let Some(Value::Table(requires)) = typed(&m, "requires", "", Ty::Table, false)? {
        for (i, v) in array(requires, "requires")?.into_iter().enumerate() {
            let Value::String(name) = v else {
                return Err(spec_err(
                    &format!("requires[{}]", i + 1),
                    "must be a string",
                ));
            };
            if !FEATURES.contains(&&*name.to_string_lossy()) {
                return Err(unsupported(&format!(
                    "required capability #{} is missing",
                    i + 1
                )));
            }
        }
    }
    typed(&m, "name", "", Ty::Str, true)?;
    options(typed(&m, "options", "", Ty::Table, false)?, "options")?;
    let Some(Value::Table(verbs)) = typed(&m, "verbs", "", Ty::Table, true)? else {
        return Ok(true);
    };
    for (key, value) in fields_any(verbs)? {
        let at = join("verbs", &key);
        match value {
            Value::Table(t) => verb(&t, &at)?,
            _ => return Err(spec_err(&at, "must be a table")),
        }
    }
    Ok(true)
}

fn fields_any(t: &Table) -> Result<BTreeMap<String, Value>, Rejection> {
    let mut out = BTreeMap::new();
    for pair in t.pairs::<Value, Value>() {
        let Ok((Value::String(key), value)) = pair else {
            return Err(spec_err("verbs", "verb names must be strings"));
        };
        out.insert(key.to_string_lossy(), value);
    }
    Ok(out)
}

/// Generic text for combination errors found by the shared semantic checks.
pub fn combination() -> Rejection {
    spec_err(
        "spec",
        "invalid combination: reserved name, duplicate id or short, bad token, or positional order",
    )
}
