//! Local, trusted image publication of bounded cluster facts.
//!
//! There is deliberately no wire setter: the Lua image that runs on this node
//! publishes directly into the registry, and only the authenticated cluster
//! listener can read the facts back as a sidecar response. The binding is
//! fenced by image generation and by the exact live session instance.
use crate::text;
use mlua::{Lua, Table, Value};
use remuda_core::session_enrichment::{safe_fact_text, ButlerFacts, FieldState};
use std::sync::Arc;

struct FactsGenerationGuard {
    registry: Arc<remuda_core::Registry>,
    generation: u64,
}

impl Drop for FactsGenerationGuard {
    fn drop(&mut self) {
        let _ = self.registry.clear_cluster_facts_for(self.generation);
    }
}

pub(crate) fn dispose(lua: &Lua) {
    drop(lua.remove_app_data::<FactsGenerationGuard>());
}

pub(crate) fn install(
    lua: &Lua,
    table: &Table,
    registry: Arc<remuda_core::Registry>,
) -> mlua::Result<()> {
    let generation = registry.begin_cluster_facts_generation();
    lua.set_app_data(FactsGenerationGuard {
        registry: registry.clone(),
        generation,
    });
    table.set(
        "_session_set_display",
        lua.create_function(
            move |_, (name, instance, value): (String, String, Option<Table>)| {
                let facts = value.map(decode).transpose()?;
                registry
                    .set_cluster_facts_for(generation, &name, &instance, facts)
                    .map(|()| true)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )
}

const FIELDS: &[&str] = &[
    "version",
    "provider",
    "member_id",
    "leader_id",
    "alias",
    "kind",
    "model",
    "context_used",
    "context_window",
    "context_percent",
];

fn decode(table: Table) -> mlua::Result<ButlerFacts> {
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
    if integer(&table, "version", u64::from(u32::MAX))? != Some(1) {
        return Err(mlua::Error::runtime("unsupported session display version"));
    }
    if text(&table, "provider")?.as_deref() != Some("butler") {
        return Err(mlua::Error::runtime("unsupported session display provider"));
    }
    let member_id = text(&table, "member_id")?
        .ok_or_else(|| mlua::Error::runtime("session display member_id is required"))?;
    if member_id.is_empty() || !safe_fact_text(&member_id) {
        return Err(mlua::Error::runtime("invalid session display member_id"));
    }
    let facts = ButlerFacts {
        member_id,
        leader_id: text(&table, "leader_id")?,
        alias: text(&table, "alias")?,
        kind: field_text(&table, "kind")?,
        model: field_text(&table, "model")?,
        context_used: field_number(&table, "context_used")?,
        context_window: field_number(&table, "context_window")?,
        context_percent: match integer(&table, "context_percent", 100)? {
            Some(percent) => FieldState::Value(percent as u8),
            None => FieldState::Unavailable,
        },
    };
    facts
        .validate()
        .map_err(|_| mlua::Error::runtime("invalid session display facts"))?;
    Ok(facts)
}

fn field_text(table: &Table, key: &str) -> mlua::Result<FieldState<String>> {
    Ok(match text(table, key)? {
        Some(value) => FieldState::Value(value),
        None => FieldState::Unavailable,
    })
}

fn field_number(table: &Table, key: &str) -> mlua::Result<FieldState<u64>> {
    Ok(match integer(table, key, u64::MAX)? {
        Some(value) => FieldState::Value(value),
        None => FieldState::Unavailable,
    })
}

fn text(table: &Table, key: &str) -> mlua::Result<Option<String>> {
    match table.raw_get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(value) => {
            let bytes = value.as_bytes();
            if bytes.len() > 128 || !safe_fact_text(&value.to_str()?) {
                return Err(mlua::Error::runtime(
                    "session display text must be a safe string of at most 128 bytes",
                ));
            }
            let text = value.to_str()?.to_string();
            if text != text::strip_terminal_controls(&text).into_owned() {
                return Err(mlua::Error::runtime(
                    "session display text contains terminal controls",
                ));
            }
            Ok(Some(text))
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

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::{ManualClock, Registry, ScriptedAgent, Session};

    fn registry_with(name: &str) -> (Arc<Registry>, String) {
        let registry = Arc::new(Registry::with_cluster_facts_clock(Arc::new(
            ManualClock::new(),
        )));
        let session = registry
            .register(Session::new(
                name,
                Box::new(ScriptedAgent::new(vec![])),
                Arc::new(ManualClock::new()),
            ))
            .unwrap();
        let instance = session.instance_id().to_string();
        (registry, instance)
    }

    /// The exact Lua table shape the Butler publisher emits must be accepted by
    /// the core setter and read back through the sidecar store.
    #[test]
    fn publisher_shape_is_accepted_for_the_exact_instance() {
        let (registry, instance) = registry_with("worker");
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        install(&lua, &table, Arc::clone(&registry)).unwrap();
        lua.globals().set("host", table).unwrap();
        lua.globals().set("instance", instance.clone()).unwrap();
        let result: bool = lua
            .load(
                r#"
                return host._session_set_display("worker", instance, {
                  version = 1, provider = "butler",
                  member_id = "private-MID", leader_id = "private-LEAD",
                  alias = "worker", kind = "codex", model = "configured-model",
                  context_used = 42000, context_window = 100000, context_percent = 42,
                })
                "#,
            )
            .eval()
            .unwrap();
        assert!(result);
        let rows = registry.cluster_facts_now();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].3.member_id, "private-MID");
        assert_eq!(rows[0].3.leader_id.as_deref(), Some("private-LEAD"));
        assert_eq!(rows[0].3.context_percent, FieldState::Value(42));

        // Exact-instance fence and private-field refusal.
        assert!(lua
            .load(
                r#"return host._session_set_display("worker", "other-instance", {version=1, provider="butler", member_id="M"})"#,
            )
            .eval::<bool>()
            .is_err());
        assert!(lua
            .load(
                r#"return host._session_set_display("worker", instance, {version=1, provider="butler", member_id="M", task="secret"})"#,
            )
            .eval::<bool>()
            .is_err());
        assert!(lua
            .load(
                r#"return host._session_set_display("worker", instance, {version=1, provider="butler", member_id="M", context_percent=101})"#,
            )
            .eval::<bool>()
            .is_err());

        // A nil clear removes only the exact instance, and image disposal clears
        // the remaining publication.
        assert!(lua
            .load(r#"return host._session_set_display("worker", instance, nil)"#)
            .eval::<bool>()
            .unwrap());
        assert!(registry.cluster_facts_now().is_empty());
        lua.load(
            r#"return host._session_set_display("worker", instance, {version=1, provider="butler", member_id="M"})"#,
        )
        .eval::<bool>()
        .unwrap();
        dispose(&lua);
        assert!(registry.cluster_facts_now().is_empty());
    }
}
