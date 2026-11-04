//! The restricted frontend Lua VM and its table conversion boundary.
//!
//! Only base, table, string, utf8 and math are loaded. I/O, OS, package,
//! debug, coroutine, `load`/`loadfile`/`dofile`, `print`, `warn`, `pcall` and
//! `xpcall` are removed before any source runs: a caught budget error must
//! never let a document be accepted, and a loop that catches forever must not
//! hang the frontend. The only host functions are pure display helpers
//! (`cell_width`, `fit_text`) with bounded input.

use super::geometry::Axis;
use super::schema::{RawDoc, RawGroup, RawLayout, RawResult, RawRow, RawSession, RawSpan};
use super::{fit_text, text_width, Color, Model};
use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Table, Value, VmState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Largest string the pure helpers will measure.
const MAX_HELPER_TEXT: usize = 64 * 1024;
/// The hook fires every this many VM instructions.
const HOOK_STEP: u64 = 10_000;
/// Checks the wall clock every this many decoded entries.
const DECODE_TICK: usize = 256;

struct Budget {
    limit: u64,
    count: u64,
    deadline: Instant,
}

impl Budget {
    fn new(limit: u64, wall_ms: u64) -> Self {
        Self {
            limit,
            count: 0,
            deadline: Instant::now() + Duration::from_millis(wall_ms),
        }
    }

    fn expired(&self) -> bool {
        self.count > self.limit || Instant::now() > self.deadline
    }
}

pub struct Vm {
    lua: Lua,
    budget: Arc<Mutex<Budget>>,
    exhausted: Arc<AtomicBool>,
}

impl Vm {
    pub fn load(sources: &[(&str, &str)], limit: u64, wall_ms: u64) -> Result<Self, String> {
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH,
            LuaOptions::default(),
        )
        .map_err(|error| format!("cannot create Lua: {error}"))?;
        lua.set_memory_limit(super::MEMORY_LIMIT_BYTES)
            .map_err(|error| format!("cannot set Lua memory limit: {error}"))?;
        let budget = Arc::new(Mutex::new(Budget::new(limit, wall_ms)));
        let exhausted = Arc::new(AtomicBool::new(false));
        let hook_budget = budget.clone();
        let hook_exhausted = exhausted.clone();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(HOOK_STEP as u32),
            move |_, _| {
                let mut budget = hook_budget
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                budget.count = budget.count.saturating_add(HOOK_STEP);
                if budget.expired() {
                    hook_exhausted.store(true, Ordering::SeqCst);
                    return Err(mlua::Error::runtime("cluster ui budget exceeded"));
                }
                Ok(VmState::Continue)
            },
        )
        .map_err(|error| format!("cannot set Lua hook: {error}"))?;
        let vm = Self {
            lua,
            budget,
            exhausted,
        };
        vm.install_helpers()?;
        vm.remove_dangerous_globals()?;
        for (name, source) in sources {
            if vm.exhausted.load(Ordering::SeqCst) {
                return Err("cluster ui budget was exhausted while loading".to_string());
            }
            vm.reset(limit, wall_ms);
            vm.lua
                .load(*source)
                .set_name(format!("@{name}"))
                .exec()
                .map_err(|error| format!("{name}: {error}"))?;
            if vm.exhausted.load(Ordering::SeqCst) {
                return Err("cluster ui budget was exhausted while loading".to_string());
            }
        }
        vm.require_compose()?;
        Ok(vm)
    }

    /// Create a fresh VM from the same source list. Used to fall back without
    /// risking state left by a failed user document.
    pub fn reload_with(&self, sources: &[(&str, &str)]) -> Result<Self, String> {
        Self::load(sources, super::LOAD_INSTRUCTIONS, super::LOAD_WALL_MS)
    }

    fn install_helpers(&self) -> Result<(), String> {
        let globals = self.lua.globals();
        let helper_budget = self.budget.clone();
        let helper_exhausted = self.exhausted.clone();
        globals
            .set(
                "cell_width",
                self.lua
                    .create_function(move |_, text: Value| {
                        if helper_exhausted.load(Ordering::SeqCst) {
                            return Err(mlua::Error::runtime("cluster ui budget exceeded"));
                        }
                        let expired = helper_budget
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .expired();
                        if expired {
                            helper_exhausted.store(true, Ordering::SeqCst);
                            return Err(mlua::Error::runtime("cluster ui budget exceeded"));
                        }
                        match text {
                            Value::String(text) => {
                                let text = text
                                    .to_str()
                                    .map_err(|_| mlua::Error::runtime("text is not UTF-8"))?;
                                if text.len() > MAX_HELPER_TEXT {
                                    return Err(mlua::Error::runtime("text is too long"));
                                }
                                Ok(text_width(&text))
                            }
                            other => Err(mlua::Error::runtime(format!(
                                "cell_width expects a string, got {}",
                                other.type_name()
                            ))),
                        }
                    })
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
        let helper_budget = self.budget.clone();
        let helper_exhausted = self.exhausted.clone();
        globals
            .set(
                "fit_text",
                self.lua
                    .create_function(move |_, (text, cells): (Value, Value)| {
                        if helper_exhausted.load(Ordering::SeqCst) {
                            return Err(mlua::Error::runtime("cluster ui budget exceeded"));
                        }
                        let expired = helper_budget
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .expired();
                        if expired {
                            helper_exhausted.store(true, Ordering::SeqCst);
                            return Err(mlua::Error::runtime("cluster ui budget exceeded"));
                        }
                        let Value::String(text) = text else {
                            return Err(mlua::Error::runtime("fit_text expects a string"));
                        };
                        let text = text
                            .to_str()
                            .map_err(|_| mlua::Error::runtime("text is not UTF-8"))?;
                        if text.len() > MAX_HELPER_TEXT {
                            return Err(mlua::Error::runtime("text is too long"));
                        }
                        let cells = match cells {
                            Value::Integer(cells) if cells >= 0 => cells as usize,
                            _ => {
                                return Err(mlua::Error::runtime(
                                    "fit_text expects a non-negative integer",
                                ));
                            }
                        };
                        Ok(fit_text(&text, cells))
                    })
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn remove_dangerous_globals(&self) -> Result<(), String> {
        let globals = self.lua.globals();
        for name in [
            "load",
            "loadfile",
            "dofile",
            "print",
            "warn",
            "require",
            "collectgarbage",
            "pcall",
            "xpcall",
        ] {
            globals
                .raw_set(name, Value::Nil)
                .map_err(|error| error.to_string())?;
        }
        // The Lua pattern engine can backtrack for seconds on a short string.
        // The VM hook cannot preempt C code, so remove the pattern verbs before
        // any source loads. The string metatable reads this same table, so
        // `("a"):find` is gone as well.
        let string: Table = globals
            .raw_get("string")
            .map_err(|error| error.to_string())?;
        for name in ["find", "match", "gmatch", "gsub"] {
            string
                .raw_set(name, Value::Nil)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn require_compose(&self) -> Result<(), String> {
        let value: Value = self
            .lua
            .globals()
            .raw_get("compose")
            .map_err(|error| error.to_string())?;
        if !matches!(value, Value::Function(_)) {
            return Err("cluster_ui.lua must define compose(model)".to_string());
        }
        Ok(())
    }

    fn reset(&self, limit: u64, wall_ms: u64) {
        let mut budget = self
            .budget
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        budget.limit = limit;
        budget.count = 0;
        budget.deadline = Instant::now() + Duration::from_millis(wall_ms);
    }

    pub fn compose(
        &mut self,
        model: &Model,
        limit: u64,
        wall_ms: u64,
    ) -> Result<RawResult, String> {
        if self.exhausted.load(Ordering::SeqCst) {
            return Err("cluster ui budget was exhausted earlier".to_string());
        }
        self.reset(limit, wall_ms);
        let started = Instant::now();
        let input = model_table(&self.lua, model).map_err(|error| error.to_string())?;
        if started.elapsed() > Duration::from_millis(wall_ms) {
            self.exhausted.store(true, Ordering::SeqCst);
            return Err("cluster ui budget exceeded while building the snapshot".to_string());
        }
        let compose: Function = self
            .lua
            .globals()
            .raw_get("compose")
            .map_err(|error| error.to_string())?;
        let value: Value = compose.call(input).map_err(|error| error.to_string())?;
        if self.exhausted.load(Ordering::SeqCst) {
            return Err("cluster ui budget was exhausted".to_string());
        }
        let table = match value {
            Value::Table(table) => table,
            other => {
                return Err(format!(
                    "compose must return a table, got {}",
                    other.type_name()
                ));
            }
        };
        let mut decode = Decode {
            tables: 0,
            rows: 0,
            spans: 0,
            bytes: 0,
            deadline: started + Duration::from_millis(wall_ms),
        };
        let value = doc_from_table(&table, &mut decode)?;
        if self.exhausted.load(Ordering::SeqCst) {
            return Err("cluster ui budget was exhausted".to_string());
        }
        let encoded_len = serde_json::to_vec(&value)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if started.elapsed() > Duration::from_millis(wall_ms) {
            self.exhausted.store(true, Ordering::SeqCst);
            return Err("cluster ui budget exceeded while validating the result".to_string());
        }
        Ok(RawResult { value, encoded_len })
    }
}

struct Decode {
    tables: usize,
    rows: usize,
    spans: usize,
    bytes: usize,
    deadline: Instant,
}

impl Decode {
    fn table(&mut self) -> Result<(), String> {
        self.tables += 1;
        if self.tables > super::MAX_TOTAL_TABLES {
            return Err("composition has too many tables".to_string());
        }
        self.tick()
    }

    fn rows(&mut self, count: usize) -> Result<(), String> {
        self.rows = self.rows.saturating_add(count);
        if self.rows > super::MAX_ROWS {
            return Err("composition has too many rows".to_string());
        }
        Ok(())
    }

    fn spans(&mut self, count: usize) -> Result<(), String> {
        self.spans = self.spans.saturating_add(count);
        if self.spans > super::MAX_TOTAL_SPANS {
            return Err("composition has too many spans".to_string());
        }
        Ok(())
    }

    fn bytes(&mut self, count: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(count);
        if self.bytes > super::MAX_OUTPUT_BYTES {
            return Err("composition text is larger than 1 MiB".to_string());
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<(), String> {
        if self.tables.is_multiple_of(DECODE_TICK) && Instant::now() > self.deadline {
            return Err("composition decode budget exceeded".to_string());
        }
        Ok(())
    }
}

fn model_table(lua: &Lua, model: &Model) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let statuses: std::collections::HashMap<u32, &str> = model
        .statuses
        .iter()
        .map(|(id, text)| (*id, text.as_str()))
        .collect();
    table.set("version", model.version)?;
    table.set("surface", model.surface.as_str())?;
    let viewport = lua.create_table()?;
    viewport.set("cols", model.viewport.cols)?;
    viewport.set("rows", model.viewport.rows)?;
    viewport.set("body_rows", model.viewport.body_rows)?;
    viewport.set("gutter_cells", model.viewport.gutter_cells)?;
    table.set("viewport", viewport)?;
    match model.selected {
        Some(selected) => table.set("selected", selected)?,
        None => table.set("selected", Value::Nil)?,
    }
    if let Some(counts) = model.counts {
        let counts_table = lua.create_table()?;
        counts_table.set("reachable", counts.reachable)?;
        counts_table.set("total", counts.total)?;
        table.set("counts", counts_table)?;
    }
    table.set("searching", model.searching)?;
    table.set("attention_only", model.attention_only)?;
    table.set("all_empty", model.all_empty)?;
    match model.preferred_list_cells {
        Some(cells) => table.set("preferred_list_cells", cells)?,
        None => table.set("preferred_list_cells", Value::Nil)?,
    }
    let groups = lua.create_table()?;
    for (index, group) in model.groups.iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("id", group.id)?;
        entry.set(
            "kind",
            match group.kind {
                super::GroupKind::Local => "local",
                super::GroupKind::Node => "node",
                super::GroupKind::Peer => "peer",
            },
        )?;
        entry.set("selected", group.selected)?;
        entry.set("expanded", group.expanded)?;
        entry.set("label", group.label.as_str())?;
        match group.state {
            Some(state) => entry.set("state", state.as_str())?,
            None => entry.set("state", Value::Nil)?,
        }
        match group.age_seconds {
            Some(age) => entry.set("age_seconds", age)?,
            None => entry.set("age_seconds", Value::Nil)?,
        }
        entry.set("pending", group.pending)?;
        entry.set("status_cells", group.status_cells)?;
        entry.set(
            "status",
            statuses
                .get(&group.id)
                .copied()
                .map(str::to_string)
                .or_else(|| model.status_for(group.id))
                .unwrap_or_default(),
        )?;
        let sessions = lua.create_table()?;
        for (session_index, session) in group.sessions.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", session.id)?;
            row.set("label", session.label.as_str())?;
            row.set("selected", session.selected)?;
            row.set("alive", session.alive)?;
            row.set("pending", session.pending)?;
            row.set("status_cells", session.status_cells)?;
            if let Some(display) = &session.display {
                row.set("display", display_table(lua, display)?)?;
            }

            row.set(
                "status",
                statuses
                    .get(&session.id)
                    .copied()
                    .map(str::to_string)
                    .or_else(|| model.status_for(session.id))
                    .unwrap_or_default(),
            )?;
            sessions.set(session_index + 1, row)?;
        }
        entry.set("sessions", sessions)?;
        groups.set(index + 1, entry)?;
    }
    table.set("groups", groups)?;
    Ok(table)
}

fn doc_from_table(table: &Table, decode: &mut Decode) -> Result<RawDoc, String> {
    if table.metatable().is_some() {
        return Err("composition result must be a raw table".to_string());
    }
    decode.table()?;
    let version = integer(raw_get(table, "version")?, "version")?;
    if !(0..=u32::MAX as i64).contains(&version) {
        return Err("version is out of range".to_string());
    }
    let title = rows(raw_get(table, "title")?, "title", decode)?;
    let groups = groups(raw_get(table, "groups")?, "groups", decode)?;
    let empty = rows(raw_get(table, "empty")?, "empty", decode)?;
    let layout = layout(raw_get(table, "layout")?, "layout", decode)?;
    Ok(RawDoc {
        version: version as u32,
        title,
        groups,
        empty,
        layout,
    })
}

fn groups(value: Value, name: &str, decode: &mut Decode) -> Result<Vec<RawGroup>, String> {
    let table = expect_table(value, name, decode)?;
    let values = array(&table, name, super::MAX_ROWS, decode)?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let table = expect_table(value, "group", decode)?;
        let id = integer(raw_get(&table, "id")?, "group id")?;
        if !(0..=u32::MAX as i64).contains(&id) {
            return Err("group id is out of range".to_string());
        }
        let rows = rows(raw_get(&table, "rows")?, "group rows", decode)?;
        let sessions = sessions(raw_get(&table, "sessions")?, "group sessions", decode)?;
        out.push(RawGroup {
            id: id as u32,
            rows,
            sessions,
        });
    }
    Ok(out)
}

fn sessions(value: Value, name: &str, decode: &mut Decode) -> Result<Vec<RawSession>, String> {
    let table = expect_table(value, name, decode)?;
    let values = array(&table, name, super::MAX_ROWS, decode)?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let table = expect_table(value, "session", decode)?;
        let id = integer(raw_get(&table, "id")?, "session id")?;
        if !(0..=u32::MAX as i64).contains(&id) {
            return Err("session id is out of range".to_string());
        }
        let rows = rows(raw_get(&table, "rows")?, "session rows", decode)?;
        out.push(RawSession {
            id: id as u32,
            rows,
        });
    }
    Ok(out)
}

fn rows(value: Value, name: &str, decode: &mut Decode) -> Result<Vec<RawRow>, String> {
    let table = expect_table(value, name, decode)?;
    let values = array(&table, name, super::MAX_ROWS, decode)?;
    decode.rows(values.len())?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let table = expect_table(value, "row", decode)?;
        out.push(RawRow {
            spans: spans(raw_get(&table, "spans")?, "row spans", decode)?,
        });
    }
    Ok(out)
}

fn spans(value: Value, name: &str, decode: &mut Decode) -> Result<Vec<RawSpan>, String> {
    let table = expect_table(value, name, decode)?;
    let values = array(&table, name, super::MAX_SPANS_PER_ROW, decode)?;
    decode.spans(values.len())?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let table = expect_table(value, "span", decode)?;
        let field = match raw_get(&table, "field")? {
            Value::Nil => None,
            Value::String(text) => {
                let text = text
                    .to_str()
                    .map_err(|_| "span field is not UTF-8".to_string())?;
                decode.bytes(text.len())?;
                if text.len() > 16 {
                    return Err("span field is not recognized".to_string());
                }
                Some(text.to_string())
            }
            _ => return Err("span field must be a string".to_string()),
        };
        let text = match raw_get(&table, "text")? {
            Value::Nil if field.is_some() => String::new(),
            Value::String(text) => {
                let text = text
                    .to_str()
                    .map_err(|_| "span text is not UTF-8".to_string())?;
                if text.len() > super::MAX_SPAN_BYTES {
                    return Err("span text is too long".to_string());
                }
                decode.bytes(text.len())?;
                text.to_string()
            }
            _ => return Err("span text must be a string".to_string()),
        };
        out.push(RawSpan {
            text,
            field,
            fg: color(raw_get(&table, "fg")?, "fg", decode)?,
            bg: color(raw_get(&table, "bg")?, "bg", decode)?,
            bold: boolean(raw_get(&table, "bold")?, "bold")?,
            dim: boolean(raw_get(&table, "dim")?, "dim")?,
            italic: boolean(raw_get(&table, "italic")?, "italic")?,
            underline: boolean(raw_get(&table, "underline")?, "underline")?,
            reverse: boolean(raw_get(&table, "reverse")?, "reverse")?,
        });
    }
    Ok(out)
}

fn layout(value: Value, name: &str, decode: &mut Decode) -> Result<RawLayout, String> {
    let table = expect_table(value, name, decode)?;
    let axis_text = bounded_string(raw_get(&table, "axis")?, "axis", 16, decode)?;
    let axis = match axis_text.as_str() {
        "horizontal" => Axis::Horizontal,
        "vertical" => Axis::Vertical,
        _ => return Err("axis must be horizontal or vertical".to_string()),
    };
    let list_cells = integer(raw_get(&table, "list_cells")?, "list_cells")?;
    if !(0..=u16::MAX as i64).contains(&list_cells) {
        return Err("list_cells is out of range".to_string());
    }
    let divider = match raw_get(&table, "divider")? {
        Value::Nil => None,
        Value::String(text) => {
            let text = text
                .to_str()
                .map_err(|_| "divider is not UTF-8".to_string())?;
            decode.bytes(text.len())?;
            if text.len() > 16 {
                return Err("divider is too long".to_string());
            }
            Some(text.to_string())
        }
        _ => return Err("divider must be a string".to_string()),
    };
    Ok(RawLayout {
        axis,
        list_cells: list_cells as u16,
        divider,
    })
}

fn color(value: Value, name: &str, decode: &mut Decode) -> Result<Option<Color>, String> {
    let table = match value {
        Value::Nil => return Ok(None),
        Value::Table(table) => table,
        _ => return Err(format!("{name} must be a color table")),
    };
    if table.metatable().is_some() {
        return Err(format!("{name} must be a raw table"));
    }
    decode.table()?;
    let index = raw_get(&table, "index")?;
    let rgb = raw_get(&table, "rgb")?;
    let index_present = !matches!(index, Value::Nil);
    let rgb_present = !matches!(rgb, Value::Nil);
    if index_present == rgb_present {
        return Err(format!("{name} needs exactly one of index or rgb"));
    }
    if index_present {
        let index = integer(index, "color index")?;
        if !(0..=255).contains(&index) {
            return Err("color index is out of range".to_string());
        }
        return Ok(Some(Color::Index(index as u8)));
    }
    let rgb = expect_table(rgb, "rgb", decode)?;
    let values = [
        raw_get(&rgb, "r")?,
        raw_get(&rgb, "g")?,
        raw_get(&rgb, "b")?,
    ];
    let mut parts = [0u8; 3];
    for (slot, value) in parts.iter_mut().zip(values) {
        let value = integer(value, "rgb component")?;
        if !(0..=255).contains(&value) {
            return Err("rgb component is out of range".to_string());
        }
        *slot = value as u8;
    }
    Ok(Some(Color::Rgb(parts[0], parts[1], parts[2])))
}

fn expect_table(value: Value, name: &str, decode: &mut Decode) -> Result<Table, String> {
    match value {
        Value::Table(table) => {
            if table.metatable().is_some() {
                return Err(format!("{name} must be a raw table, not a metatable"));
            }
            decode.table()?;
            Ok(table)
        }
        other => Err(format!("{name} must be a table, got {}", other.type_name())),
    }
}

fn raw_get(table: &Table, name: &str) -> Result<Value, String> {
    table
        .raw_get(name)
        .map_err(|error| format!("{name}: {error}"))
}

fn integer(value: Value, name: &str) -> Result<i64, String> {
    match value {
        Value::Integer(value) => Ok(value),
        other => Err(format!(
            "{name} must be an integer, got {}",
            other.type_name()
        )),
    }
}

fn bounded_string(
    value: Value,
    name: &str,
    max: usize,
    decode: &mut Decode,
) -> Result<String, String> {
    let Value::String(value) = value else {
        return Err(format!("{name} must be a string"));
    };
    let text = value.to_str().map_err(|_| format!("{name} is not UTF-8"))?;
    decode.bytes(text.len())?;
    if text.len() > max {
        return Err(format!("{name} is too long"));
    }
    Ok(text.to_string())
}

fn boolean(value: Value, name: &str) -> Result<bool, String> {
    match value {
        Value::Nil => Ok(false),
        Value::Boolean(value) => Ok(value),
        other => Err(format!(
            "{name} must be a boolean, got {}",
            other.type_name()
        )),
    }
}

fn array(table: &Table, name: &str, cap: usize, decode: &mut Decode) -> Result<Vec<Value>, String> {
    let len = table.raw_len();
    if len > cap {
        return Err(format!("{name} has too many entries"));
    }
    let mut out = Vec::with_capacity(len);
    for index in 1..=len {
        let value: Value = table
            .raw_get(index)
            .map_err(|error| format!("{name}[{index}]: {error}"))?;
        if matches!(value, Value::Nil) {
            return Err(format!("{name} is sparse"));
        }
        out.push(value);
    }
    let mut count = 0usize;
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|error| format!("{name}: {error}"))?;
        count += 1;
        if count > len {
            return Err(format!("{name} must be a contiguous array"));
        }
        let valid = matches!(key, Value::Integer(index) if index >= 1 && index as usize <= len);
        if !valid {
            return Err(format!("{name} must be a contiguous array"));
        }
    }
    if count != len {
        return Err(format!("{name} must be a contiguous array"));
    }
    decode.tick()?;
    Ok(out)
}

fn display_table(lua: &Lua, display: &super::DisplayFacts) -> mlua::Result<Table> {
    let facts = lua.create_table()?;
    facts.set("provider", display.provider.as_str())?;
    facts.set("alias", display.alias.as_deref())?;
    facts.set("leader", display.leader.as_deref())?;
    facts.set("kind", display.kind.as_deref())?;
    facts.set("model", display.model.as_deref())?;
    facts.set("context_percent", display.context_percent)?;
    facts.set("context_used", display.context_used)?;
    facts.set("context_window", display.context_window)?;
    Ok(facts)
}
