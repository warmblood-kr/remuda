//! Local, trusted image publication of bounded display facts. There is no
//! wire setter: authenticated List readers only receive an expiring copy.

use mlua::{Lua, Table, Value};
use remuda_core::session_display::SessionDisplay;
use std::sync::Arc;

struct DisplayGenerationGuard {
    registry: Arc<remuda_core::Registry>,
    generation: u64,
}

impl Drop for DisplayGenerationGuard {
    fn drop(&mut self) {
        let _ = self.registry.clear_session_displays_for(self.generation);
    }
}

pub(crate) fn dispose(lua: &Lua) {
    drop(lua.remove_app_data::<DisplayGenerationGuard>());
}

pub(crate) fn install(
    lua: &Lua,
    table: &Table,
    registry: Arc<remuda_core::Registry>,
) -> mlua::Result<()> {
    let generation = registry.begin_display_generation();
    lua.set_app_data(DisplayGenerationGuard {
        registry: registry.clone(),
        generation,
    });
    table.set(
        "_session_set_display",
        lua.create_function(
            move |_, (name, instance, value): (String, String, Option<Table>)| {
                let display = value.map(decode).transpose()?;
                registry
                    .set_session_display_for(generation, &name, &instance, display)
                    .map(|()| true)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )
}

fn decode(table: Table) -> mlua::Result<SessionDisplay> {
    const FIELDS: &[&str] = &[
        "version",
        "provider",
        "member_id",
        "leader_id",
        "alias",
        "leader",
        "kind",
        "model",
        "context_used",
        "context_window",
        "context_percent",
    ];
    for (index, pair) in table.clone().pairs::<Value, Value>().enumerate() {
        if index >= FIELDS.len() {
            return Err(mlua::Error::runtime("too many session display fields"));
        }
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(mlua::Error::runtime("unknown session display field"));
        };
        if !FIELDS.contains(&key.to_str()?.as_ref()) {
            return Err(mlua::Error::runtime("unknown session display field"));
        }
    }
    let display = SessionDisplay {
        version: integer(&table, "version", u64::from(u32::MAX))?.unwrap_or(0) as u32,
        provider: text(&table, "provider")?.unwrap_or_default(),
        member_id: text(&table, "member_id")?,
        leader_id: text(&table, "leader_id")?,
        alias: text(&table, "alias")?,
        leader: text(&table, "leader")?,
        kind: text(&table, "kind")?,
        model: text(&table, "model")?,
        context_used: integer(&table, "context_used", u64::MAX)?,
        context_window: integer(&table, "context_window", u64::MAX)?,
        context_percent: integer(&table, "context_percent", 100)?.map(|n| n as u16),
        valid_for_ms: 10_000,
    };
    Ok(display)
}

fn text(table: &Table, key: &str) -> mlua::Result<Option<String>> {
    match table.raw_get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(value) if value.as_bytes().len() <= 128 => {
            Ok(Some(value.to_str()?.to_string()))
        }
        _ => Err(mlua::Error::runtime(
            "session display text must be a string of at most 128 bytes",
        )),
    }
}

fn integer(table: &Table, key: &str, maximum: u64) -> mlua::Result<Option<u64>> {
    let number = match table.raw_get::<Value>(key)? {
        Value::Nil => return Ok(None),
        Value::Integer(value) if value >= 0 => value as u64,
        Value::Number(value)
            if value.is_finite()
                && value >= 0.0
                && value.fract() == 0.0
                && value <= 9_007_199_254_740_991.0 =>
        {
            value as u64
        }
        _ => {
            return Err(mlua::Error::runtime(
                "session display number must be a nonnegative integer",
            ))
        }
    };
    if number > maximum {
        return Err(mlua::Error::runtime("session display number out of range"));
    }
    Ok(Some(number))
}

/// Bind a source observation time to the exact status payload, atomically.
/// Used only by the owning Codex observer; publication never reads this path.
pub fn write_observation(status: &str, payload: &str) -> std::io::Result<()> {
    let observed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    crate::fs_atomic::write_atomic_lua_private(
        std::path::Path::new(&format!("{status}.observed")),
        format!("{observed}\n{payload}").as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::{ManualClock, Registry, ScriptedAgent, Session};

    #[test]
    fn old_peers_and_semantically_invalid_display_keep_the_base_session() {
        let registry = Registry::new();
        registry
            .register(Session::new(
                "work",
                Box::new(ScriptedAgent::new(vec![])),
                Arc::new(ManualClock::new()),
            ))
            .unwrap();
        let base = serde_json::to_value(&registry.list()[0]).unwrap();
        assert!(base.get("display").is_none());
        for invalid in [
            serde_json::json!(true),
            serde_json::json!([{"nested":true}]),
            serde_json::json!({"version":2,"provider":"butler","valid_for_ms":1000}),
            serde_json::json!({"version":1,"provider":"other","valid_for_ms":1000}),
            serde_json::json!({"version":1,"provider":"butler","valid_for_ms":1000,"model":"\u{1b}]bad"}),
            serde_json::json!({"version":1,"provider":"butler","valid_for_ms":1000,"context_percent":"42"}),
            serde_json::json!({"version":1,"provider":"butler","valid_for_ms":1000,"task":{"secret":[1,2,3]}}),
        ] {
            let mut wire = base.clone();
            wire["display"] = invalid;
            let summary: remuda_core::SessionSummary = serde_json::from_value(wire).unwrap();
            assert_eq!(summary.name, "work");
            assert_eq!(summary.instance_id, registry.list()[0].instance_id);
            assert!(summary.display.is_none());
        }
        #[derive(serde::Deserialize)]
        struct OldReader {
            name: String,
        }
        let mut enriched = base;
        enriched["display"] =
            serde_json::json!({"version":1,"provider":"butler","valid_for_ms":1000});
        let old: OldReader = serde_json::from_value(enriched).unwrap();
        assert_eq!(old.name, "work");
    }

    #[test]
    fn old_lua_drop_cannot_clear_a_new_image_publication() {
        let registry = Arc::new(Registry::with_display_clock(Arc::new(ManualClock::new())));
        let session = registry
            .register(Session::new(
                "work",
                Box::new(ScriptedAgent::new(vec![])),
                Arc::new(ManualClock::new()),
            ))
            .unwrap();
        let old = Lua::new();
        install(&old, &old.create_table().unwrap(), registry.clone()).unwrap();
        let new = Lua::new();
        let table = new.create_table().unwrap();
        install(&new, &table, registry.clone()).unwrap();
        new.globals().set("host", table).unwrap();
        new.globals()
            .set("instance", session.instance_id())
            .unwrap();
        new.load("host._session_set_display('work',instance,{version=1,provider='butler'})")
            .exec()
            .unwrap();
        drop(old);
        assert!(registry.list()[0].display.is_some());
        drop(new);
        assert!(
            registry.list()[0].display.is_none(),
            "Lua exit disposes without StopModules"
        );
    }

    #[test]
    fn local_publication_is_typed_instance_bound_and_disposed_with_image() {
        let registry = Arc::new(Registry::with_display_clock(Arc::new(ManualClock::new())));
        let session = registry
            .register(Session::new(
                "work",
                Box::new(ScriptedAgent::new(vec![])),
                Arc::new(ManualClock::new()),
            ))
            .unwrap();
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        install(&lua, &table, registry.clone()).unwrap();
        lua.globals().set("host", table).unwrap();
        lua.globals()
            .set("instance", session.instance_id())
            .unwrap();
        assert!(lua.load("return host._session_set_display('work', instance, {version=1,provider='butler',kind='codex'})")
            .eval::<bool>().unwrap());
        assert_eq!(
            registry.list()[0].display.as_ref().unwrap().kind.as_deref(),
            Some("codex")
        );
        for input in [
            "{version=1,provider='butler',task='secret'}",
            "{version=1,provider='butler',model=string.rep('x',129)}",
            "{version=1,provider='butler',context_percent=101}",
            "{version='1',provider='butler'}",
            "{version=1,provider='butler',model=42}",
            "{version=1,provider='butler',context_percent='42'}",
            "{version=1,provider='butler',context_percent=true}",
            "{version=1,provider='butler',context_percent={}}",
            "{version=1,provider='butler',context_percent=0/0}",
            "{version=1,provider='butler',model='\\27]bad'}",
        ] {
            assert!(lua
                .load(format!(
                    "return host._session_set_display('work',instance,{input})"
                ))
                .eval::<bool>()
                .is_err());
        }
        assert!(lua
            .load("return host._session_set_display('work','old',{version=1,provider='butler'})")
            .eval::<bool>()
            .is_err());
        assert!(
            registry.list()[0].display.is_some(),
            "invalid writes replace nothing"
        );
        dispose(&lua);
        assert!(registry.list()[0].display.is_none());
        assert!(
            lua.load(
                "return host._session_set_display('work',instance,{version=1,provider='butler'})"
            )
            .eval::<bool>()
            .is_err(),
            "disposed callbacks cannot republish"
        );
    }
}
