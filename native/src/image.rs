//! The image: one Lua interpreter that lives as long as the daemon.
//!
//! # Why a thread and a channel, and not a mutex around `Lua`
//!
//! `mlua::Lua` is `!Send` without the `send` feature — a Lua state is
//! single-threaded, and no lock changes that. So the state is *pinned* to one
//! thread that owns it outright, and every other thread reaches it by posting a
//! job and waiting for the answer. A thread serving a queue is an event loop.
//!
//! Unlike Emacs, a long-running script freezes nothing else: sessions here are
//! not Lua objects but Rust structs behind their own locks, pumped by threads
//! that never touch Lua. A script that loops forever makes only the *next Lua
//! caller* wait — pty output keeps being read, `remuda ls` keeps answering.
//!
//! ⚠ The corollary, named now so it is not discovered as a deadlock later: an
//! output pump may never *call into* Lua. If `on_output(session, fn)` is ever
//! added, the pump must post a job to this loop, not run the callback itself.

use crate::script;
use mlua::Lua;
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

/// One unit of work for the image: source to evaluate, and where the answer
/// goes. The reply channel is per-job rather than shared, so two callers
/// waiting at once cannot receive each other's result.
struct Job {
    code: String,
    name: Option<String>,
    reply: Sender<Result<String, String>>,
}

/// A handle to the daemon's Lua image. Cloneable and `Send`; the interpreter
/// itself never leaves its thread.
#[derive(Clone)]
pub struct Image {
    jobs: Sender<Job>,
}

impl Image {
    /// Start the interpreter and return a handle to it. `socket` is the
    /// daemon's own, for bindings that act on a session by name (attach,
    /// capture, ...).
    // `registry` lets a read-only binding like `ls` answer in-process
    // instead of looping a request back over that same socket. `counters`
    // is the daemon's own named `Counters`, shared with `Ticker`.
    pub fn spawn(
        socket: &Path,
        registry: Arc<remuda_core::Registry>,
        counters: Arc<crate::tick::Counters>,
    ) -> Self {
        let (jobs, inbox) = channel::<Job>();
        let socket: PathBuf = socket.to_path_buf();
        let image = Self { jobs };
        let handle = image.clone();

        std::thread::spawn(move || {
            let lua = Lua::new();
            // Everything `print` writes during one job, so it can travel back
            // to whoever asked instead of vanishing. `Rc` rather than `Arc`
            // because this never leaves the thread — the whole reason the
            // interpreter is pinned here.
            let printed = Rc::new(RefCell::new(String::new()));

            // A failure here means no image at all, so every eval must say so
            // rather than the thread dying quietly and every caller hanging.
            let ready = script::bindings(&lua, &socket, registry, counters, handle)
                .and_then(|table| lua.globals().set("remuda", table))
                // The tool frame is Lua over those bindings, not a second set of
                // them. It must load *after* the table exists and *before* any
                // caller, so a `tools/list` on a fresh daemon is already true.
                .and_then(|()| {
                    lua.load(include_str!("tools.lua"))
                        .set_name("@remuda/tools.lua")
                        .exec()
                })
                .and_then(|()| script::hide_module_activator(&lua))
                .and_then(|()| capture_print(&lua, Rc::clone(&printed)))
                .map_err(|e| e.to_string());

            for job in inbox {
                printed.borrow_mut().clear();
                let answer = match &ready {
                    Err(why) => Err(format!("image failed to start: {why}")),
                    Ok(()) => eval(&lua, &job.code, job.name.as_deref())
                        .map(|value| join_output(&printed.borrow(), &value)),
                };
                // A caller that gave up and dropped its receiver is not an
                // error: `remuda -e` can be Ctrl-C'd mid-evaluation, and the
                // work still ran.
                let _ = job.reply.send(answer);
            }
        });

        image
    }

    /// Send `code` without waiting for it to finish — `tick.rs` needs this so
    /// its own callback's duration never blocks it or the FIFO queue behind it.
    pub fn submit(
        &self,
        code: &str,
        name: Option<&str>,
    ) -> Result<std::sync::mpsc::Receiver<Result<String, String>>, String> {
        let (reply, answer) = channel();
        self.jobs
            .send(Job {
                code: code.to_string(),
                name: name.map(str::to_string),
                reply,
            })
            .map_err(|_| "the image is not running".to_string())?;
        Ok(answer)
    }

    /// Evaluate `code` in the image and wait for the result. State persists
    /// between calls — a `-e`, a script and a REPL line are all doors into one
    /// interpreter, and a variable set by any of them outlives the call.
    pub fn eval(&self, code: &str, name: Option<&str>) -> Result<String, String> {
        self.submit(code, name)?
            .recv()
            .map_err(|_| "the image stopped without answering".to_string())?
    }
}

/// Evaluate one chunk, expression-first: `return <code>` is tried before plain
/// `<code>` (the reference REPL's `addreturn` trick), so `-e "1+1"` prints `2`
/// and `-e "x = 1"` still works. Results are `tostring`ed and tab-joined.
fn eval(lua: &Lua, code: &str, name: Option<&str>) -> Result<String, String> {
    // Lua's own convention: `@` means "this is a filename", `=` means "use
    // this verbatim". A script keeps the path it came from so a traceback
    // names a file an editor can jump to; a `-e` or REPL line has no file, and
    // `(eval)` is what it should be called instead of a quoted copy of itself.
    let chunk = name.map_or_else(|| "=(eval)".to_string(), |n| format!("@{n}"));

    // Only a compile failure means "not an expression" — a runtime error in
    // an expression must not run the code a second time as statements (#128).
    let function = match lua
        .load(format!("return {code}"))
        .set_name(&chunk)
        .into_function()
    {
        Ok(function) => function,
        // Not an expression. Run it as statements, and report *that* error if
        // it fails — reporting the expression-parse error instead would name a
        // `return` the user never wrote, which is the single most confusing
        // thing this function could do.
        Err(_) => lua
            .load(code)
            .set_name(&chunk)
            .into_function()
            .map_err(|e| e.to_string())?,
    };
    let values = function
        .call::<mlua::MultiValue>(())
        .map_err(|e| e.to_string())?;

    let rendered: Vec<String> = values.iter().map(render).collect();
    Ok(rendered.join("\t"))
}

/// Point `print` at a buffer instead of the daemon's stdout, which is
/// `Stdio::null()` — without this a script's `print` vanishes and the caller
/// sees nothing. Lua semantics kept: `tostring`ed, tab-separated, newline.
fn capture_print(lua: &Lua, into: Rc<RefCell<String>>) -> mlua::Result<()> {
    let print = lua.create_function(move |_, values: mlua::MultiValue| {
        let line: Vec<String> = values.iter().map(render).collect();
        let mut buffer = into.borrow_mut();
        buffer.push_str(&line.join("\t"));
        buffer.push('\n');
        Ok(())
    })?;
    lua.globals().set("print", print)
}

/// What the caller sees: anything printed, then whatever the chunk came to.
/// Either half may be empty, and neither leaves a stray blank line behind.
fn join_output(printed: &str, value: &str) -> String {
    match (printed.trim_end_matches('\n'), value) {
        ("", value) => value.to_string(),
        (printed, "") => printed.to_string(),
        (printed, value) => format!("{printed}\n{value}"),
    }
}

/// Deliberately small: a table printed at a REPL is for reading, and anything
/// past this is a dump. Both caps elide with `…` rather than truncating
/// silently, so a short rendering never reads as a whole table.
const MAX_DEPTH: usize = 4;
const MAX_ITEMS: usize = 32;

/// One value as a Lua user expects to see it: `tostring` semantics, minus the
/// address on functions and threads — an address differs every run, so an
/// expected output could not be written down. Tables render their contents.
fn render(value: &mlua::Value) -> String {
    match value {
        mlua::Value::Nil => "nil".to_string(),
        mlua::Value::Boolean(b) => b.to_string(),
        mlua::Value::Integer(i) => i.to_string(),
        mlua::Value::Number(n) => n.to_string(),
        mlua::Value::String(s) => s.to_string_lossy().to_string(),
        mlua::Value::Table(t) => render_table(t, 0, &mut Vec::new()),
        other => format!("<{}>", other.type_name()),
    }
}

/// A value seen *inside* a table, where `1` and `"1"` must not look alike —
/// so strings are quoted here and bare at top level, keeping `tostring`
/// semantics for `print` and for `remuda -e "s"`.
fn element(value: &mlua::Value, depth: usize, seen: &mut Vec<*const c_void>) -> String {
    match value {
        mlua::Value::String(s) => format!("{:?}", s.to_string_lossy()),
        mlua::Value::Table(t) => render_table(t, depth, seen),
        other => render(other),
    }
}

/// A table by its contents. `seen` holds the tables on the path from the root,
/// so `t.self = t` renders `<cycle>` instead of recursing forever.
fn render_table(t: &mlua::Table, depth: usize, seen: &mut Vec<*const c_void>) -> String {
    let identity = t.to_pointer();
    if seen.contains(&identity) {
        return "<cycle>".to_string();
    }
    if depth >= MAX_DEPTH {
        return "{…}".to_string();
    }
    seen.push(identity);
    let body = table_body(t, depth, seen);
    seen.pop();
    body
}

/// Array part in index order, then every other key sorted — an address is not
/// the only thing that varies between runs, and Lua's hash order is the other.
fn table_body(t: &mlua::Table, depth: usize, seen: &mut Vec<*const c_void>) -> String {
    let len = t.raw_len();
    let mut items: Vec<String> = (1..=len)
        .take(MAX_ITEMS)
        .map(|i| element(&t.raw_get(i).unwrap_or(mlua::Value::Nil), depth + 1, seen))
        .collect();

    let mut keyed: Vec<(String, String, String)> = t
        .pairs::<mlua::Value, mlua::Value>()
        .flatten()
        .filter(|(k, _)| !in_array_part(k, len))
        .map(|(k, v)| {
            let sort_by = element(&k, depth + 1, seen);
            let shown = format!(
                "{} = {}",
                key(&k, depth, seen),
                element(&v, depth + 1, seen)
            );
            (k.type_name().to_string(), sort_by, shown)
        })
        .collect();
    keyed.sort();

    let elided = items.len() < len || items.len() + keyed.len() > MAX_ITEMS;
    items.extend(
        keyed
            .into_iter()
            .take(MAX_ITEMS.saturating_sub(items.len()))
            .map(|(_, _, shown)| shown),
    );
    if elided {
        items.push("…".to_string());
    }
    format!("{{{}}}", items.join(", "))
}

/// Whether `raw_len`'s array part already rendered this key.
fn in_array_part(k: &mlua::Value, len: usize) -> bool {
    matches!(k, mlua::Value::Integer(i) if *i >= 1 && (*i as usize) <= len)
}

/// `name = v` when the key is a bare Lua identifier, `["a b"] = v` otherwise —
/// both are what you would type to build the table back.
fn key(k: &mlua::Value, depth: usize, seen: &mut Vec<*const c_void>) -> String {
    if let mlua::Value::String(s) = k {
        let text = s.to_string_lossy();
        let identifier = !text.is_empty()
            && !text.starts_with(|c: char| c.is_ascii_digit())
            && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if identifier {
            return text.to_string();
        }
    }
    format!("[{}]", element(k, depth + 1, seen))
}

#[cfg(test)]
mod tests {
    use super::render;
    use mlua::Lua;

    fn lifecycle_lua() -> Lua {
        let lua = Lua::new();
        let remuda = lua.create_table().unwrap();
        remuda
            .set("_registry", lua.create_table().unwrap())
            .unwrap();
        lua.globals().set("remuda", remuda).unwrap();
        lua.load(include_str!("tools.lua")).exec().unwrap();
        lua
    }

    /// What `remuda -e <code>` would print, without a daemon in the way.
    fn shown(code: &str) -> String {
        let lua = Lua::new();
        let value: mlua::Value = lua.load(format!("return {code}")).eval().unwrap();
        render(&value)
    }

    #[test]
    fn scalars_keep_tostring_semantics() {
        assert_eq!(shown("nil"), "nil");
        assert_eq!(shown("true"), "true");
        assert_eq!(shown("42"), "42");
        assert_eq!(shown("'hello'"), "hello");
        assert_eq!(shown("print"), "<function>");
    }

    #[test]
    fn the_owners_table_shows_its_contents() {
        assert_eq!(
            shown("(function() local a = 2 return {a, 1, 'a', 1, \"a\", 2} end)()"),
            r#"{2, 1, "a", 1, "a", 2}"#
        );
    }

    #[test]
    fn a_string_is_quoted_so_it_cannot_be_read_as_a_number() {
        assert_eq!(shown("{1, '1'}"), r#"{1, "1"}"#);
    }

    #[test]
    fn keys_are_sorted_so_the_output_can_be_written_down() {
        // Insertion order differs from this on every run; the rendering must
        // not. Sorted by the key's own value, so bracketing does not reorder.
        assert_eq!(
            shown("{zulu = 1, alpha = 2, [3.5] = 3, ['two words'] = 4}"),
            r#"{[3.5] = 3, alpha = 2, ["two words"] = 4, zulu = 1}"#
        );
    }

    #[test]
    fn the_array_part_comes_first_in_index_order() {
        assert_eq!(shown("{10, 20, name = 'x'}"), r#"{10, 20, name = "x"}"#);
    }

    #[test]
    fn module_reload_replaces_owned_hooks_and_tools_but_keeps_initialized_state() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local initialized = 0
            local function declaration(step, tool_name)
              return {
                api = "remuda-module-v1",
                state_version = 1,
                initialize = function()
                  initialized = initialized + 1
                  return { count = 0 }
                end,
                hooks = {{ event = "probe", run = function(state)
                  state.count = state.count + step
                end }},
                tools = {{
                  name = tool_name,
                  about = "A sufficiently long description for this test tool.",
                  run = function(state) return tostring(state.count) end,
                }},
              }
            end
            assert(remuda._activate_module("sample", declaration(1, "sample_old")))
            remuda.emit("probe")
            assert(remuda.tools.sample_old() == "1")
            assert(remuda._activate_module("sample", declaration(10, "sample_new")))
            remuda.emit("probe")
            assert(initialized == 1)
            assert(remuda.tools.sample_old == nil)
            assert(remuda.tools.sample_new() == "11")
            assert(#remuda.hooks.probe == 1)
            "#,
        )
        .exec()
        .expect("reload should keep one hook and the original state");
    }

    #[test]
    fn failed_state_migration_keeps_the_previous_state_and_registrations() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local v1 = {
              api = "remuda-module-v1", state_version = 1,
              initialize = function() return { nested = { count = 4 } } end,
              hooks = {{ event = "probe", run = function(state)
                state.nested.count = state.nested.count + 1
              end }},
            }
            assert(remuda._activate_module("sample", v1))
            local v2 = {
              api = "remuda-module-v1", state_version = 2,
              initialize = function() error("must not initialize on reload") end,
              migrations = {[1] = function(state)
                state.nested.count = state.nested.count + 10
                return { total = state.nested.count }
              end},
              hooks = {{ event = "probe", run = function(state)
                state.total = state.total + 1
                remuda._observed_total = state.total
              end }},
            }
            assert(remuda._activate_module("sample", v2))
            local broken = {
              api = "remuda-module-v1", state_version = 3,
              initialize = function() return {} end,
              migrations = {[2] = function(state)
                state.total = -1
                error("migration failed")
              end},
              hooks = {},
            }
            local ok = pcall(remuda._activate_module, "sample", broken)
            assert(not ok)
            remuda.emit("probe")
            assert(remuda._observed_total == 15)
            assert(#remuda.hooks.probe == 1)
            "#,
        )
        .exec()
        .expect("failed migration must leave the prior module active");
    }

    #[test]
    fn contributions_are_ordered_replaced_by_id_and_handed_out_as_copies() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            remuda.contribute("p", "b", { order = 10 })
            remuda.contribute("p", "a", { order = 10 })
            remuda.contribute("p", "z", { order = -5 })
            remuda.contribute("p", "c", {})
            remuda.contribute("other", "x", {})
            local ids = {}
            for i, item in ipairs(remuda.contributions("p")) do ids[i] = item.id end
            assert(table.concat(ids, ",") == "z,c,a,b", table.concat(ids, ","))

            remuda.contribute("p", "a", { order = -10, label = "new" })
            local first = remuda.contributions("p")[1]
            assert(first.id == "a" and first.entry.label == "new" and first.owner == nil)
            assert(#remuda.contributions("p") == 4, "same id replaces")
            assert(#remuda.contributions("none") == 0)

            first.id, first.owner = "hacked", "me"
            assert(remuda.contributions("p")[1].id == "a", "wrappers are copies")
            for _, bad in ipairs({ { "", "i", {} }, { "p", "", {} }, { "p", "i", "x" },
                                   { "p", "i", { order = "1" } } }) do
              assert(not pcall(remuda.contribute, bad[1], bad[2], bad[3]), "refused: " .. tostring(bad[2]))
            end
            "#,
        )
        .exec()
        .expect("contribute/contributions");
    }

    #[test]
    fn declared_contributions_are_owned_bound_to_state_and_replaced_on_reload() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local function declaration(ids)
              local entries = {}
              for i, id in ipairs(ids) do
                entries[i] = { id = id, order = i, run = function(state, x) return state.tag .. ":" .. x end }
              end
              return { api = "remuda-module-v1", state_version = 1,
                initialize = function() return { tag = "s" } end,
                contributes = { ["host.command"] = entries } }
            end
            remuda.contribute("host.command", "foreign", { order = 99 })
            assert(remuda._activate_module("guest", declaration({ "one", "two" })))
            local list = remuda.contributions("host.command")
            assert(#list == 3 and list[1].id == "one" and list[1].owner == "guest")
            assert(list[1].entry.run("x") == "s:x", "function fields are bound to state")
            assert(list[3].id == "foreign" and list[3].owner == nil)

            assert(remuda._activate_module("guest", declaration({ "three" })))
            local ids = {}
            for i, item in ipairs(remuda.contributions("host.command")) do ids[i] = item.id end
            assert(table.concat(ids, ",") == "three,foreign", "reload replaces only its own: " .. table.concat(ids, ","))
            "#,
        )
        .exec()
        .expect("declared contributions");
    }

    #[test]
    fn a_failed_start_rolls_back_declared_contributions() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local function declaration(id)
              return { api = "remuda-module-v1", state_version = 1,
                initialize = function() return {} end,
                contributes = { p = {{ id = id }} } }
            end
            assert(remuda._activate_module("guest", declaration("old")))
            local ok, _, _, rollback = remuda._activate_module("guest", declaration("new"))
            assert(ok and remuda.contributions("p")[1].id == "new")
            rollback()
            local list = remuda.contributions("p")
            assert(#list == 1 and list[1].id == "old" and list[1].owner == "guest", "rollback restores")
            "#,
        )
        .exec()
        .expect("rollback of contributions");
    }

    #[test]
    fn invalid_declared_contributions_are_refused_before_anything_changes() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local function with(contributes)
              return { api = "remuda-module-v1", state_version = 1,
                initialize = function() return {} end, contributes = contributes }
            end
            assert(remuda._activate_module("guest", with({ p = {{ id = "kept" }} })))
            for _, bad in ipairs({ { p = {{ id = "a" }, { id = "a" }} }, { p = {{}} },
                                   { p = { "x" } }, { [""] = {{ id = "a" }} }, "x" }) do
              assert(not pcall(remuda._activate_module, "guest", with(bad)))
              local list = remuda.contributions("p")
              assert(#list == 1 and list[1].id == "kept", "a refused declaration changed nothing")
            end
            "#,
        )
        .exec()
        .expect("invalid declarations");
    }

    #[test]
    fn a_declared_contribution_owned_by_another_mod_is_refused() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local function with(id)
              return { api = "remuda-module-v1", state_version = 1,
                initialize = function() return {} end,
                contributes = { ["host.command"] = {{ id = id, label = "mine" }} } }
            end
            assert(remuda._activate_module("alpha", with("shared")))
            local ok, err = pcall(remuda._activate_module, "beta", with("shared"))
            assert(not ok, "a cross-mod replace must be refused")
            err = tostring(err)
            for _, part in ipairs({ "alpha", "beta", "host.command/shared" }) do
              assert(err:find(part, 1, true), "error must name " .. part .. ": " .. err)
            end
            local list = remuda.contributions("host.command")
            assert(#list == 1 and list[1].owner == "alpha", "the owner's entry is untouched")
            assert(remuda._activate_module("alpha", with("shared")), "the owner may still reload it")
            local replaced, err = pcall(remuda.contribute, "host.command", "shared", { label = "imperative" })
            assert(not replaced and tostring(err):find("alpha", 1, true),
              "an owner-less caller cannot replace a mod's contribution")
            assert(remuda.contributions("host.command")[1].entry.label == "mine",
              "the owner's entry remains untouched")
            "#,
        )
        .exec()
        .expect("cross-mod contribution refusal");
    }

    #[test]
    fn a_returned_entry_is_a_copy_that_cannot_reach_the_registry() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            assert(remuda._activate_module("alpha", { api = "remuda-module-v1", state_version = 1,
              initialize = function() return {} end,
              contributes = { p = {{ id = "owned", order = 5, label = "alpha" }} } }))
            remuda.contribute("p", "free", { order = 10, label = "free" })
            for _, row in ipairs(remuda.contributions("p")) do
              row.entry.order, row.entry.label = -999, "hacked"
            end
            local list = remuda.contributions("p")
            assert(list[1].id == "owned" and list[1].entry.order == 5 and list[1].entry.label == "alpha",
              "an owned entry changed through a returned row")
            assert(list[2].id == "free" and list[2].entry.order == 10 and list[2].entry.label == "free",
              "an imperative entry changed through a returned row")
            "#,
        )
        .exec()
        .expect("returned entries are copies");
    }

    #[test]
    fn a_cycle_terminates() {
        assert_eq!(
            shown("(function() local t = {} t.self = t return t end)()"),
            "{self = <cycle>}"
        );
    }

    #[test]
    fn two_references_to_one_table_are_not_a_cycle() {
        // `seen` holds the path, not everything visited — a diamond is finite.
        assert_eq!(
            shown("(function() local leaf = {1} return {leaf, leaf} end)()"),
            "{{1}, {1}}"
        );
    }

    #[test]
    fn depth_is_capped() {
        assert_eq!(shown("{{{{{1}}}}}"), "{{{{{…}}}}}");
    }

    #[test]
    fn width_is_capped() {
        let out = shown("(function() local t = {} for i = 1, 100 do t[i] = i end return t end)()");
        assert!(out.starts_with("{1, 2, 3,"), "{out}");
        assert!(out.ends_with(", 32, …}"), "{out}");
    }

    #[test]
    fn an_empty_table_is_empty() {
        assert_eq!(shown("{}"), "{}");
    }

    // #128: only a `return CODE` that fails to compile falls back to running
    // CODE as statements; an expression that errors at run time ran twice.
    #[test]
    fn an_expression_that_errors_at_run_time_runs_once() {
        let lua = Lua::new();
        lua.load("calls = 0; function boom() calls = calls + 1; error('boom') end")
            .exec()
            .unwrap();
        let error = super::eval(&lua, "boom()", None).unwrap_err();
        assert!(error.contains("boom"), "{error}");
        assert_eq!(lua.globals().get::<i64>("calls").unwrap(), 1);
        assert_eq!(super::eval(&lua, "x = 1", None).unwrap(), "");
    }
}
