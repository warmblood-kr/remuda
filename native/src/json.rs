//! Strict, bounded JSON words for the Lua image.

use mlua::{Lua, LuaString, Table, Value};
use serde::de::{self, DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value as Json};
use std::collections::HashSet;
use std::ffi::c_void;
use std::fmt;

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_VALUES: usize = 100_000;

struct JsonSeed<'a> {
    depth: usize,
    values: &'a mut usize,
}

struct JsonVisitor<'a> {
    depth: usize,
    values: &'a mut usize,
}

impl<'de> DeserializeSeed<'de> for JsonSeed<'_> {
    type Value = Json;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Json, D::Error> {
        if *self.values >= MAX_VALUES {
            return Err(D::Error::custom("maximum JSON value count exceeded"));
        }
        *self.values += 1;
        deserializer.deserialize_any(JsonVisitor {
            depth: self.depth,
            values: self.values,
        })
    }
}

impl<'de> Visitor<'de> for JsonVisitor<'_> {
    type Value = Json;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Json, E> {
        Ok(Json::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Json, E> {
        Ok(Json::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Json, E> {
        Ok(Json::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Json, E> {
        Number::from_f64(value)
            .map(Json::Number)
            .ok_or_else(|| E::custom("number is outside the finite range"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Json, E> {
        Ok(Json::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Json, E> {
        Ok(Json::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Json, A::Error> {
        self.check_depth::<A::Error>()?;
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(JsonSeed {
            depth: self.depth + 1,
            values: &mut *self.values,
        })? {
            values.push(value);
        }
        Ok(Json::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Json, A::Error> {
        self.check_depth::<A::Error>()?;
        let mut values = Map::new();
        let mut keys = HashSet::new();
        while let Some(key) = access.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(A::Error::custom("duplicate key"));
            }
            let value = access.next_value_seed(JsonSeed {
                depth: self.depth + 1,
                values: &mut *self.values,
            })?;
            values.insert(key, value);
        }
        Ok(Json::Object(values))
    }
}

impl JsonVisitor<'_> {
    fn check_depth<E: de::Error>(&self) -> Result<(), E> {
        if self.depth >= MAX_DEPTH {
            Err(E::custom("maximum nesting depth exceeded"))
        } else {
            Ok(())
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Json, String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!("input exceeds {MAX_BYTES} bytes"));
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let mut values = 0;
    let value = JsonSeed {
        depth: 0,
        values: &mut values,
    }
    .deserialize(&mut deserializer)
    .map_err(|error| {
        if error.to_string().contains("duplicate key") {
            "duplicate key".to_string()
        } else {
            error.to_string()
        }
    })?;
    deserializer.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn into_lua(
    lua: &Lua,
    value: Json,
    depth: usize,
    array_mt: &Table,
    object_mt: &Table,
) -> mlua::Result<Value> {
    match value {
        Json::Null => Ok(Value::NULL),
        Json::Bool(value) => Ok(Value::Boolean(value)),
        Json::Number(number) => number_to_lua(number),
        Json::String(value) => Ok(Value::String(lua.create_string(value)?)),
        Json::Array(values) => {
            check_encode_depth(depth)?;
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            table.set_metatable(Some(array_mt.clone()))?;
            for (index, value) in values.into_iter().enumerate() {
                table.raw_set(
                    index + 1,
                    into_lua(lua, value, depth + 1, array_mt, object_mt)?,
                )?;
            }
            Ok(Value::Table(table))
        }
        Json::Object(values) => {
            check_encode_depth(depth)?;
            let table = lua.create_table_with_capacity(0, values.len())?;
            table.set_metatable(Some(object_mt.clone()))?;
            for (key, value) in values {
                table.raw_set(key, into_lua(lua, value, depth + 1, array_mt, object_mt)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

fn number_to_lua(number: Number) -> mlua::Result<Value> {
    if let Some(integer) = number.as_i64() {
        return Ok(Value::Integer(integer));
    }
    let value = number
        .as_f64()
        .ok_or_else(|| runtime_error("number cannot be represented as a Lua number"))?;
    if !value.is_finite() {
        return Err(runtime_error("number is outside the finite range"));
    }
    Ok(Value::Number(value))
}

fn check_encode_depth(depth: usize) -> mlua::Result<()> {
    if depth >= MAX_DEPTH {
        Err(runtime_error("maximum nesting depth exceeded"))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableKind {
    Array,
    Object,
}

#[derive(Default)]
struct EncodeState {
    active: HashSet<*const c_void>,
    text_bytes: usize,
}

impl EncodeState {
    fn account_text(&mut self, bytes: usize) -> mlua::Result<()> {
        self.text_bytes = self
            .text_bytes
            .checked_add(bytes)
            .ok_or_else(|| runtime_error("input exceeds 8388608 bytes"))?;
        if self.text_bytes > MAX_BYTES {
            return Err(runtime_error("input exceeds 8388608 bytes"));
        }
        Ok(())
    }
}

fn table_kind(
    table: &Table,
    array_mt: &Table,
    object_mt: &Table,
) -> mlua::Result<Option<TableKind>> {
    let Some(metatable) = table.metatable() else {
        return Ok(None);
    };
    if metatable == *array_mt {
        Ok(Some(TableKind::Array))
    } else if metatable == *object_mt {
        Ok(Some(TableKind::Object))
    } else {
        Ok(None)
    }
}

fn ensure_taggable(table: &Table, array_mt: &Table, object_mt: &Table) -> mlua::Result<()> {
    if let Some(metatable) = table.metatable() {
        if metatable != *array_mt && metatable != *object_mt {
            return Err(runtime_error(
                "cannot replace a table's existing non-JSON metatable",
            ));
        }
    }
    Ok(())
}

fn to_json(
    value: Value,
    depth: usize,
    array_mt: &Table,
    object_mt: &Table,
    state: &mut EncodeState,
) -> mlua::Result<Json> {
    match value {
        Value::Nil => Err(runtime_error(
            "nil is not a JSON value; use remuda.json.null",
        )),
        Value::LightUserData(value) if Value::LightUserData(value) == Value::NULL => Ok(Json::Null),
        Value::LightUserData(_) => Err(runtime_error("lightuserdata is not a JSON value")),
        Value::Boolean(value) => Ok(Json::Bool(value)),
        Value::Integer(value) => Ok(Json::Number(value.into())),
        Value::Number(value) => Number::from_f64(value)
            .map(Json::Number)
            .ok_or_else(|| runtime_error("NaN and infinity are not JSON numbers")),
        Value::String(value) => {
            let text = value
                .to_str()
                .map_err(|_| runtime_error("JSON strings must be valid UTF-8"))?;
            state.account_text(text.len())?;
            Ok(Json::String(text.to_string()))
        }
        Value::Table(table) => {
            check_encode_depth(depth)?;
            let pointer = table.to_pointer();
            if !state.active.insert(pointer) {
                return Err(runtime_error("cyclic tables cannot be encoded as JSON"));
            }
            let result = table_to_json(&table, depth, array_mt, object_mt, state);
            state.active.remove(&pointer);
            result
        }
        other => Err(runtime_error(format!(
            "{} is not a JSON value",
            other.type_name()
        ))),
    }
}

fn table_to_json(
    table: &Table,
    depth: usize,
    array_mt: &Table,
    object_mt: &Table,
    state: &mut EncodeState,
) -> mlua::Result<Json> {
    let kind = table_kind(table, array_mt, object_mt)?;
    let entries = table
        .pairs::<Value, Value>()
        .collect::<mlua::Result<Vec<_>>>()?;
    if kind == Some(TableKind::Array) || (kind.is_none() && is_dense_array(&entries)) {
        return encode_array(entries, depth, array_mt, object_mt, state);
    }
    if kind.is_none() && has_only_positive_integer_keys(&entries) {
        return Err(runtime_error("sparse arrays cannot be encoded as JSON"));
    }
    encode_object(entries, kind, depth, array_mt, object_mt, state)
}

fn has_only_positive_integer_keys(entries: &[(Value, Value)]) -> bool {
    !entries.is_empty()
        && entries
            .iter()
            .all(|(key, _)| matches!(key, Value::Integer(index) if *index > 0))
}

fn is_dense_array(entries: &[(Value, Value)]) -> bool {
    if entries.is_empty() {
        return false;
    }
    let mut indexes = Vec::with_capacity(entries.len());
    for (key, _) in entries {
        let Value::Integer(index) = key else {
            return false;
        };
        if *index < 1 {
            return false;
        }
        indexes.push(*index as usize);
    }
    indexes.sort_unstable();
    indexes.iter().copied().eq(1..=entries.len())
}

fn encode_array(
    entries: Vec<(Value, Value)>,
    depth: usize,
    array_mt: &Table,
    object_mt: &Table,
    state: &mut EncodeState,
) -> mlua::Result<Json> {
    let mut values: Vec<(usize, Value)> = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let Value::Integer(index) = key else {
            return Err(runtime_error(
                "JSON arrays must contain only positive integer keys",
            ));
        };
        if index < 1 {
            return Err(runtime_error(
                "JSON arrays must contain only positive integer keys",
            ));
        }
        values.push((index as usize, value));
    }
    values.sort_by_key(|(index, _)| *index);
    if values
        .iter()
        .enumerate()
        .any(|(n, (index, _))| n + 1 != *index)
    {
        return Err(runtime_error(
            "JSON arrays must be dense with keys from 1 to n",
        ));
    }
    let mut encoded = Vec::with_capacity(values.len());
    for (_, value) in values {
        encoded.push(to_json(value, depth + 1, array_mt, object_mt, state)?);
    }
    Ok(Json::Array(encoded))
}

fn encode_object(
    entries: Vec<(Value, Value)>,
    kind: Option<TableKind>,
    depth: usize,
    array_mt: &Table,
    object_mt: &Table,
    state: &mut EncodeState,
) -> mlua::Result<Json> {
    if entries.is_empty() && kind.is_none() {
        return Err(runtime_error(
            "empty tables are ambiguous; use remuda.json.array or remuda.json.object",
        ));
    }
    let mut object = Map::new();
    for (key, value) in entries {
        let Value::String(key) = key else {
            return Err(runtime_error("JSON objects must contain only string keys"));
        };
        let key = key
            .to_str()
            .map_err(|_| runtime_error("JSON object keys must be valid UTF-8"))?
            .to_string();
        state.account_text(key.len())?;
        object.insert(key, to_json(value, depth + 1, array_mt, object_mt, state)?);
    }
    Ok(Json::Object(object))
}

fn runtime_error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

/// Build `remuda.json`, with private table-kind metatables captured by its words.
pub fn bindings(lua: &Lua) -> mlua::Result<Table> {
    let namespace = lua.create_table()?;
    let array_mt = tag_metatable(lua, "json array")?;
    let object_mt = tag_metatable(lua, "json object")?;
    let array_tag = array_mt.clone();
    let object_tag = object_mt.clone();
    namespace.set("null", Value::NULL)?;
    let object_tag_for_array = object_mt.clone();
    namespace.set(
        "array",
        lua.create_function(move |_, table: Table| {
            ensure_taggable(&table, &array_tag, &object_tag_for_array)?;
            table.set_metatable(Some(array_tag.clone()))?;
            Ok(table)
        })?,
    )?;
    let array_tag_for_object = array_mt.clone();
    namespace.set(
        "object",
        lua.create_function(move |_, table: Table| {
            ensure_taggable(&table, &array_tag_for_object, &object_tag)?;
            table.set_metatable(Some(object_tag.clone()))?;
            Ok(table)
        })?,
    )?;
    bind_decode(lua, &namespace, array_mt.clone(), object_mt.clone())?;
    bind_encode(lua, &namespace, array_mt, object_mt)?;
    Ok(namespace)
}

fn tag_metatable(lua: &Lua, label: &str) -> mlua::Result<Table> {
    let metatable = lua.create_table()?;
    metatable.set("__metatable", label)?;
    Ok(metatable)
}

fn bind_decode(
    lua: &Lua,
    namespace: &Table,
    array_mt: Table,
    object_mt: Table,
) -> mlua::Result<()> {
    namespace.set(
        "decode",
        lua.create_function(move |lua, text: LuaString| match parse(&text.as_bytes()) {
            Ok(value) => Ok((into_lua(lua, value, 0, &array_mt, &object_mt)?, Value::Nil)),
            Err(error) => Ok((Value::Nil, Value::String(lua.create_string(error)?))),
        })?,
    )
}

fn bind_encode(
    lua: &Lua,
    namespace: &Table,
    array_mt: Table,
    object_mt: Table,
) -> mlua::Result<()> {
    namespace.set(
        "encode",
        lua.create_function(move |_, (value, options): (Value, Option<Table>)| {
            let pretty = options
                .as_ref()
                .map(|options| options.get::<Option<bool>>("pretty"))
                .transpose()?
                .flatten()
                .unwrap_or(false);
            let value = to_json(value, 0, &array_mt, &object_mt, &mut EncodeState::default())?;
            let text = if pretty {
                serde_json::to_string_pretty(&value)
            } else {
                serde_json::to_string(&value)
            }
            .map_err(|error| runtime_error(error.to_string()))?;
            if text.len() > MAX_BYTES {
                return Err(runtime_error(format!("output exceeds {MAX_BYTES} bytes")));
            }
            Ok(text)
        })?,
    )
}
