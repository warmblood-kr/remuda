//! Strict validation of opt-in v2 Lua specs, before any clap command is built.
//! All reads are raw (no `__index`/`__pairs`) and messages never echo values.

use crate::cli_parse::{ArgSpec, OptionSpec, Spec, VerbSpec, FEATURES};
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
    let mut odd_key = false;
    for pair in t.pairs::<Value, Value>() {
        match pair {
            Ok((Value::String(key), value)) => out.insert(key.to_string_lossy(), value),
            _ => {
                odd_key = true;
                None
            }
        };
    }
    // Classify after collecting so the first problem does not depend on hash order.
    if let Some(key) = out.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(spec_err(&join(path, key), "unknown field"));
    }
    if odd_key {
        return Err(spec_err(&shown(path), "field names must be strings"));
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

/// Version gate: `Ok(true)` only for an explicit integer `version = 2`.
/// `report_version = 2` alone never selects strict validation (G8a behavior).
fn gate(t: &Table) -> Result<bool, Rejection> {
    let version = t
        .raw_get::<Value>("version")
        .map_err(|_| spec_err("version", "unreadable"))?;
    let strict = match version {
        Value::Nil | Value::Integer(1) => false,
        Value::Integer(2) => true,
        Value::Integer(n) if n > 2 => {
            return Err(unsupported("spec version is newer than this core supports"))
        }
        _ => return Err(spec_err("version", "must be an integer 1 or 2")),
    };
    if !strict {
        return Ok(false);
    }
    match t
        .raw_get::<Value>("report_version")
        .map_err(|_| spec_err("report_version", "unreadable"))?
    {
        Value::Nil | Value::Integer(1) | Value::Integer(2) => {}
        Value::Integer(n) if n > 2 => {
            return Err(unsupported(
                "report version is newer than this core supports",
            ))
        }
        _ => return Err(spec_err("report_version", "must be an integer 1 or 2")),
    }
    Ok(true)
}

fn text(v: &Value, at: &str) -> Result<String, Rejection> {
    match v {
        Value::String(s) => s
            .to_str()
            .map(|s| s.to_string())
            .map_err(|_| spec_err(at, "must be valid text")),
        _ => Err(spec_err(at, "has the wrong type")),
    }
}

fn str_of(
    m: &BTreeMap<String, Value>,
    key: &str,
    at: &str,
    required: bool,
) -> Result<Option<String>, Rejection> {
    typed(m, key, at, Ty::Str, required)?
        .map(|v| text(v, &join(at, key)))
        .transpose()
}

fn flag(m: &BTreeMap<String, Value>, key: &str, at: &str) -> Result<Option<bool>, Rejection> {
    Ok(typed(m, key, at, Ty::Bool, false)?.map(|v| matches!(v, Value::Boolean(true))))
}

fn options(v: Option<&Value>, path: &str) -> Result<Vec<OptionSpec>, Rejection> {
    let Some(Value::Table(t)) = v else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (at, item) in tables(t, path)? {
        let m = fields(&item, &at, &["long", "short", "value", "help", "global"])?;
        let short = match str_of(&m, "short", &at, false)? {
            None => None,
            Some(s) => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(c),
                    _ => return Err(spec_err(&join(&at, "short"), "must be one character")),
                }
            }
        };
        out.push(OptionSpec {
            long: str_of(&m, "long", &at, true)?.unwrap_or_default(),
            short,
            value: str_of(&m, "value", &at, false)?,
            help: str_of(&m, "help", &at, true)?.unwrap_or_default(),
            global: flag(&m, "global", &at)?.unwrap_or(false),
        });
    }
    Ok(out)
}

fn verb(name: &str, t: &Table, path: &str) -> Result<VerbSpec, Rejection> {
    let m = fields(t, path, &["about", "next", "args", "options"])?;
    let mut args = Vec::new();
    if let Some(Value::Table(list)) = typed(&m, "args", path, Ty::Table, false)? {
        for (at, item) in tables(list, &join(path, "args"))? {
            let a = fields(&item, &at, &["name", "help", "multiple", "required"])?;
            args.push(ArgSpec {
                name: str_of(&a, "name", &at, true)?.unwrap_or_default(),
                help: str_of(&a, "help", &at, true)?.unwrap_or_default(),
                multiple: flag(&a, "multiple", &at)?.unwrap_or(false),
                required: flag(&a, "required", &at)?.unwrap_or(true),
            });
        }
    }
    Ok(VerbSpec {
        name: name.to_owned(),
        about: str_of(&m, "about", path, false)?.unwrap_or_default(),
        next: str_of(&m, "next", path, true)?.unwrap_or_default(),
        args,
        options: options(
            typed(&m, "options", path, Ty::Table, false)?,
            &join(path, "options"),
        )?,
    })
}

/// Validate a spec table and decode it from raw reads only (no metamethods run).
/// `Ok(None)` means a legacy spec: nothing was read beyond the version gate.
pub fn check(t: &Table) -> Result<Option<Spec>, Rejection> {
    if !gate(t)? {
        return Ok(None);
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
    let name = str_of(&m, "name", "", true)?.unwrap_or_default();
    let options = options(typed(&m, "options", "", Ty::Table, false)?, "options")?;
    let mut verbs = Vec::new();
    if let Some(Value::Table(list)) = typed(&m, "verbs", "", Ty::Table, true)? {
        for (key, value) in fields_any(list)? {
            let at = join("verbs", &key);
            match value {
                Value::Table(vt) => verbs.push(verb(&key, &vt, &at)?),
                _ => return Err(spec_err(&at, "must be a table")),
            }
        }
    }
    let spec = Spec {
        name,
        options,
        verbs,
    };
    crate::script::validate_cli_spec(&spec).map_err(|_| combination())?;
    Ok(Some(spec))
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
fn combination() -> Rejection {
    spec_err(
        "spec",
        "invalid combination: reserved name, duplicate id or short, bad token, or positional order",
    )
}
