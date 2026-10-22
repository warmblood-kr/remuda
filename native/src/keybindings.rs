//! Lua-owned UI key declarations, consumed as immutable native snapshots.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

pub const LIST_CONTEXT: &str = "session.list.browse";
pub const TOGGLE_LIST: &str = "session.toggle_list";
/// Open the line composer for the selected session.
pub const COMPOSE: &str = "session.compose";

/// The single native capability catalog. Keys and help belong to Lua.
fn validate(context: &str, key: &str, action: Option<&str>) -> Result<String, String> {
    let actions: &[&str] = match context {
        LIST_CONTEXT => &[TOGGLE_LIST, COMPOSE],
        "cluster.tree" => &[
            "cluster.detach",
            "cluster.close",
            "cluster.search",
            "cluster.attention",
            "cluster.remote.keys",
            "cluster.up",
            "cluster.down",
            "cluster.left",
            "cluster.right",
            "cluster.select",
        ],
        "cluster.search" => &[
            "cluster.search.clear",
            "cluster.search.delete",
            "cluster.up",
            "cluster.down",
            "cluster.left",
            "cluster.right",
            "cluster.select",
        ],
        "cluster.composer" => &[
            "cluster.composer.submit",
            "cluster.composer.clear",
            "cluster.composer.back",
            "cluster.composer.detach",
            "cluster.composer.left",
            "cluster.composer.right",
            "cluster.composer.forward",
            "cluster.composer.backward",
            "cluster.composer.up",
            "cluster.composer.down",
            "cluster.composer.home",
            "cluster.composer.home_emacs",
            "cluster.composer.end_emacs",
            "cluster.composer.kill",
            "cluster.composer.end",
            "cluster.composer.backspace",
            "cluster.composer.delete",
        ],
        "cluster.confirm" => &["cluster.confirm.accept", "cluster.confirm.accept_upper"],
        "cluster.remote.keys" => &["cluster.remote.back"],
        "cluster.runtime" => &["cluster.redraw"],
        _ => return Err(format!("unknown key context: {context}")),
    };
    if action.is_some_and(|action| !actions.contains(&action)) {
        return Err(format!("unknown action: {}", action.unwrap()));
    }
    let event = normalize_event(parse_key(key)?);
    // Legacy browse handlers ignore most modifiers. Reserve their entire
    // event code until each command has migrated to the registry.
    if context == LIST_CONTEXT
        && matches!(
            event.code,
            KeyCode::Char('c' | 'q' | 'k' | 'j' | 'h' | 'n' | 'x' | 'y' | 'v' | 'p' | 'i')
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::End
                | KeyCode::Enter
        )
    {
        return Err(format!("key conflicts with a legacy browse command: {key}"));
    }
    canonical_event(event).ok_or_else(|| format!("unsupported key: {key}"))
}

fn validate_help(help: &str) -> Result<(), String> {
    if help.is_empty() || help.chars().any(char::is_control) {
        return Err("binding needs printable help".into());
    }
    Ok(())
}

fn help_validator(lua: &mlua::Lua) -> mlua::Result<mlua::Function> {
    lua.create_function(|_, help: String| validate_help(&help).map_err(mlua::Error::runtime))
}

fn parse_key(spec: &str) -> Result<KeyEvent, String> {
    let mut base = spec;
    let mut modifiers = KeyModifiers::NONE;
    loop {
        if let Some(rest) = base.strip_prefix("C-") {
            modifiers |= KeyModifiers::CONTROL;
            base = rest;
        } else if let Some(rest) = base.strip_prefix("M-") {
            modifiers |= KeyModifiers::ALT;
            base = rest;
        } else {
            break;
        }
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        base = match base {
            "4" => "\\",
            "5" => "]",
            "6" => "^",
            "7" => "_",
            _ => base,
        };
    }
    let normalized = format!(
        "{}{}{base}",
        if modifiers.contains(KeyModifiers::CONTROL) {
            "C-"
        } else {
            ""
        },
        if modifiers.contains(KeyModifiers::ALT) {
            "M-"
        } else {
            ""
        }
    );
    if remuda_core::keys::key(&normalized).is_none() {
        return Err(format!("unknown key: {spec}"));
    }
    let code = match base.trim_matches(['<', '>']) {
        "NUL" => KeyCode::Null,
        "LFD" | "linefeed" | "RET" | "return" | "enter" => KeyCode::Enter,
        "TAB" | "tab" => KeyCode::Tab,
        "SPC" | "space" => KeyCode::Char(' '),
        "ESC" | "escape" | "esc" => KeyCode::Esc,
        "DEL" | "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        "prior" | "pageup" => KeyCode::PageUp,
        "next" | "pagedown" => KeyCode::PageDown,
        "backtab" => KeyCode::BackTab,
        name if name.starts_with('f') && name.len() > 1 => KeyCode::F(
            name[1..]
                .parse()
                .map_err(|_| format!("unknown key: {spec}"))?,
        ),
        _ => {
            let mut chars = base.chars();
            let c = chars.next().ok_or_else(|| format!("unknown key: {spec}"))?;
            if chars.next().is_some() {
                return Err(format!("unsupported key: {spec}"));
            }
            KeyCode::Char(c)
        }
    };
    Ok(KeyEvent::new(code, modifiers))
}

pub fn canonical_key(spec: &str) -> Result<String, String> {
    canonical_event(parse_key(spec)?).ok_or_else(|| format!("unknown key: {spec}"))
}

fn normalize_event(mut event: KeyEvent) -> KeyEvent {
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = event.code {
            event.code = match c.to_ascii_lowercase() {
                'h' | '?' => KeyCode::Backspace,
                'i' => KeyCode::Tab,
                'j' | 'm' => KeyCode::Enter,
                '[' => KeyCode::Esc,
                '@' | ' ' => KeyCode::Null,
                '4' => KeyCode::Char('\\'),
                '5' => KeyCode::Char(']'),
                '6' => KeyCode::Char('^'),
                '7' => KeyCode::Char('_'),
                c => KeyCode::Char(c),
            };
            if !matches!(event.code, KeyCode::Char(_)) {
                event.modifiers.remove(KeyModifiers::CONTROL);
            }
        }
    }
    event
}

fn canonical_event(event: KeyEvent) -> Option<String> {
    let event = normalize_event(event);
    let mut modifiers = event.modifiers;
    // Crossterm already applies Shift to characters, but preserves it on
    // named keys. Refuse named Shift chords until a supported grammar exists.
    if matches!(event.code, KeyCode::Char(_) | KeyCode::BackTab) {
        modifiers.remove(KeyModifiers::SHIFT);
    }
    if !(modifiers - KeyModifiers::CONTROL - KeyModifiers::ALT).is_empty() {
        return None;
    }
    let base = match event.code {
        KeyCode::Char('4' | '\\') if modifiers.contains(KeyModifiers::CONTROL) => "\\".into(),
        KeyCode::Char(' ') => "SPC".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Null => "NUL".into(),
        KeyCode::Enter => "RET".into(),
        KeyCode::Tab => "TAB".into(),
        KeyCode::Backspace => "DEL".into(),
        KeyCode::Esc => "ESC".into(),
        KeyCode::Up => "<up>".into(),
        KeyCode::Down => "<down>".into(),
        KeyCode::Left => "<left>".into(),
        KeyCode::Right => "<right>".into(),
        KeyCode::Home => "<home>".into(),
        KeyCode::End => "<end>".into(),
        KeyCode::Insert => "<insert>".into(),
        KeyCode::Delete => "<delete>".into(),
        KeyCode::PageUp => "<prior>".into(),
        KeyCode::PageDown => "<next>".into(),
        KeyCode::BackTab => "<backtab>".into(),
        KeyCode::F(n) => format!("<f{n}>"),
        _ => return None,
    };
    Some(format!(
        "{}{}{base}",
        if modifiers.contains(KeyModifiers::CONTROL) {
            "C-"
        } else {
            ""
        },
        if modifiers.contains(KeyModifiers::ALT) {
            "M-"
        } else {
            ""
        }
    ))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    context: String,
    key: String,
    action: String,
    help: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub generation: u64,
    pub ready: bool,
    pub diagnostic: Option<String>,
    rows: Vec<Binding>,
}

impl Snapshot {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let mut snapshot: Self = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let mut keys = std::collections::HashSet::new();
        let mut actions = std::collections::HashSet::new();
        for row in &mut snapshot.rows {
            row.key = validate(&row.context, &row.key, Some(&row.action))?;
            validate_help(&row.help)?;
            if !keys.insert((row.context.clone(), row.key.clone()))
                || !actions.insert((row.context.clone(), row.action.clone()))
            {
                return Err("conflicting snapshot binding".into());
            }
        }
        Ok(snapshot)
    }

    pub fn resolve(&self, context: &str, event: KeyEvent) -> Option<&str> {
        let key = canonical_event(event)?;
        self.rows
            .iter()
            .find(|row| row.context == context && row.key == key)
            .map(|row| row.action.as_str())
    }

    pub fn help(&self, context: &str) -> String {
        self.rows
            .iter()
            .filter(|row| row.context == context)
            .map(|row| format!("{} {}", row.key, row.help))
            .collect::<Vec<_>>()
            .join("   ")
    }
    pub fn display_help(&self, context: &str, actions: &[&str]) -> String {
        self.rows
            .iter()
            .filter(|row| {
                row.context == context
                    && (actions.is_empty() || actions.contains(&row.action.as_str()))
            })
            .map(|row| format!("{} {}", key_label(&row.key), row.help))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

fn key_label(key: &str) -> String {
    let mut base = key;
    let mut prefix = String::new();
    while let Some(rest) = base.strip_prefix("C-").or_else(|| base.strip_prefix("M-")) {
        prefix.push_str(if base.starts_with("C-") {
            "Ctrl-"
        } else {
            "Alt-"
        });
        base = rest;
    }
    let label = match base {
        "RET" => "Enter",
        "ESC" => "Esc",
        "DEL" => "Backspace",
        "SPC" => "Space",
        "TAB" => "Tab",
        "<up>" => "↑",
        "<down>" => "↓",
        "<left>" => "←",
        "<right>" => "→",
        "<home>" => "Home",
        "<end>" => "End",
        "<delete>" => "Delete",
        "<insert>" => "Insert",
        "<prior>" => "PageUp",
        "<next>" => "PageDown",
        "<backtab>" => "Shift-Tab",
        other => other,
    };
    if key.starts_with("C-") && base.len() == 1 {
        format!("{prefix}{}", label.to_ascii_uppercase())
    } else {
        format!("{prefix}{label}")
    }
}

#[cfg(test)]
pub(crate) fn test_snapshot(overrides: &str) -> Snapshot {
    let lua = mlua::Lua::new();
    let table = lua.create_table().unwrap();
    install(&lua, &table, || (true, None)).unwrap();
    table.set("_registry", lua.create_table().unwrap()).unwrap();
    table
        .set("json", crate::json::bindings(&lua).unwrap())
        .unwrap();
    lua.globals().set("remuda", table).unwrap();
    lua.load(include_str!("tools.lua")).exec().unwrap();
    lua.load(overrides).exec().unwrap();
    let wire = lua.load("local s=remuda.keymap_snapshot(); s.rows=remuda.json.array(s.rows); return remuda.json.encode(s)").eval::<String>().unwrap();
    Snapshot::from_json(&wire).unwrap()
}

/// A bounded client-side readiness wait; neither the listener nor Lua thread
/// ever waits for user configuration that may need to call back into them.
pub fn fetch(path: &Path, timeout: Duration) -> Result<Snapshot, String> {
    wait_for_snapshot(timeout, |remaining| {
        let request = remuda_core::protocol::Request::Eval {
            code: "local s = remuda.keymap_snapshot(); s.rows = remuda.json.array(s.rows); return remuda.json.encode(s)".into(),
            name: Some("@remuda/keymap-snapshot".into()),
        };
        match crate::client::request_with_timeout(path, &request, remaining)
            .map_err(|e| e.to_string())?
        {
            remuda_core::protocol::Response::Value(json) => Snapshot::from_json(&json),
            remuda_core::protocol::Response::Error(error) => Err(error),
            _ => Err("unexpected keymap reply".into()),
        }
    })
}

fn wait_for_snapshot(
    timeout: Duration,
    mut read: impl FnMut(Duration) -> Result<Snapshot, String>,
) -> Result<Snapshot, String> {
    let started = Instant::now();
    loop {
        let timeout = timeout.saturating_sub(started.elapsed());
        if timeout.is_zero() {
            return Err("user configuration did not finish before the keymap deadline".into());
        }
        let snapshot = read(timeout)?;
        if snapshot.ready {
            return Ok(snapshot);
        }
        std::thread::sleep(timeout.min(Duration::from_millis(20)));
    }
}

pub(crate) fn install(
    lua: &mlua::Lua,
    table: &mlua::Table,
    status: impl Fn() -> (bool, Option<String>) + Send + 'static,
) -> mlua::Result<()> {
    table.set(
        "_keymap_factory",
        lua.load(include_str!("keybindings.lua"))
            .set_name("@remuda/keybindings.lua")
            .eval::<mlua::Function>()?,
    )?;
    table.set(
        "_keymap_validate",
        lua.create_function(
            |_, (context, key, action): (String, String, Option<String>)| {
                validate(&context, &key, action.as_deref()).map_err(mlua::Error::runtime)
            },
        )?,
    )?;
    table.set("_keymap_validate_help", help_validator(lua)?)?;
    table.set(
        "_keymap_config_status",
        lua.create_function(move |_, ()| Ok(status()))?,
    )
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    fn lua() -> Lua {
        let lua = Lua::new();
        lua.load("remuda = { _registry = {} }").exec().unwrap();
        let table = lua.globals().get::<mlua::Table>("remuda").unwrap();
        super::install(&lua, &table, || (true, None)).unwrap();
        let factory = lua
            .load(include_str!("keybindings.lua"))
            .eval::<mlua::Function>()
            .unwrap();
        lua.globals()
            .get::<mlua::Table>("remuda")
            .unwrap()
            .set("_keymap_factory", factory)
            .unwrap();
        lua.globals()
            .get::<mlua::Table>("remuda")
            .unwrap()
            .set(
                "_keymap_validate_help",
                super::help_validator(&lua).unwrap(),
            )
            .unwrap();
        lua.load(include_str!("tools.lua")).exec().unwrap();
        lua.load(
            r#"
            local snapshot = remuda.keymap_snapshot
            remuda.keymap_snapshot = function()
                local s, rows = snapshot(), {}
                for _, row in ipairs(s.rows) do
                    if row.context == 'session.list.browse' then rows[#rows+1] = row end
                end
                s.rows = rows
                return s
            end
        "#,
        )
        .exec()
        .unwrap();
        lua
    }

    #[test]
    fn keybindings_help_controls_are_rejected_before_committing() {
        lua().load(r#"
            remuda.unbind('session.list.browse', 'l')
            remuda.unbind('session.list.browse', 'm')
            local before = remuda.keymap_snapshot().generation
            for _, help in ipairs({'bad\0help', 'bad\27help', 'bad\194\128help', 'bad\194\159help'}) do
                local ok = pcall(remuda.define_key, {context='session.list.browse', key='z', action='session.toggle_list', help=help})
                assert(not ok, 'control character help must be refused')
                assert(remuda.keymap_snapshot().generation == before, 'failed declaration must not commit')
                assert(#remuda.keymap_snapshot().rows == 0)
            end
            remuda.define_key{context='session.list.browse', key='z', action='session.toggle_list', help='목록 …'}
            assert(remuda.keymap_snapshot().rows[1].help == '목록 …')
        "#).exec().unwrap();
    }

    #[test]
    fn keybindings_lifecycle_reload_and_failed_start_restore_owned_bindings() {
        lua().load(r#"
            local function declaration(key, fail)
                return {api='remuda-module-v1', state_version=1,
                    initialize=function() return {} end,
                    start=function()
                        remuda.rebind('session.list.browse', 'session.toggle_list', key)
                        if fail then error('start failed') end
                    end}
            end
            local function toggle() for _, r in ipairs(remuda.keymap_snapshot().rows) do if r.action == 'session.toggle_list' then return r end end end
            local ok, state, start = remuda._activate_module('guest', declaration('z'))
            assert(ok); start(state)
            assert(toggle().key == 'z')
            local ok2, state2, start2, rollback = remuda._activate_module('guest', declaration('M-l', true))
            assert(ok2); assert(not pcall(start2, state2)); rollback()
            assert(toggle().key == 'z')
            local ok3, _, _, undo = remuda._activate_module('guest', {api='remuda-module-v1', state_version=1, initialize=function() return {} end})
            assert(ok3); assert(toggle().key == 'l')
            undo()
            assert(toggle().key == 'z')
            local other_ok, other_state, other_start = remuda._activate_module('other', declaration('M-l'))
            assert(other_ok); assert(not pcall(other_start, other_state))
            remuda.rebind('session.list.browse', 'session.toggle_list', 'M-l')
            local ok4, state4, start4 = remuda._activate_module('guest', declaration('z'))
            assert(ok4); start4(state4)
            assert(toggle().key == 'M-l', 'user override survives mod reload')
        "#).exec().unwrap();
    }

    #[test]
    fn keybindings_rebind_updates_the_effective_help_and_removes_old_key() {
        lua()
            .load(
                r#"
            local function toggle() for _, r in ipairs(remuda.keymap_snapshot().rows) do if r.action == 'session.toggle_list' then return r end end end
            local before = remuda.keymap_snapshot()
            assert(toggle().key == 'l')
            remuda.rebind('session.list.browse', 'session.toggle_list', 'M-l')
            local after = remuda.keymap_snapshot()
            assert(#after.rows == #before.rows)
            assert(toggle().key == 'M-l' and toggle().help == 'list')
            assert(after.generation > before.generation)
            remuda.unbind('session.list.browse', 'M-l')
            assert(#remuda.keymap_snapshot().rows == #before.rows - 1)
            assert(toggle() == nil)
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn keybindings_reject_unknowns_and_conflicts_without_mutating_snapshot() {
        lua().load(r#"
            local generation = remuda.keymap_snapshot().generation
            for _, spec in ipairs({
                {context='missing', key='x', action='session.toggle_list', help='list'},
                {context='session.list.browse', key='x', action='missing', help='list'},
                {context='session.list.browse', key='unknown-key', action='session.toggle_list', help='list'},
                {context='session.list.browse', key='l', action='session.toggle_list', help='duplicate'},
                {context='session.list.browse', key='q', action='session.toggle_list', help='list'},
            }) do assert(not pcall(remuda.define_key, spec)) end
            assert(not pcall(remuda.rebind, 'session.list.browse', 'session.toggle_list', 'q'))
            assert(remuda.keymap_snapshot().generation == generation)
            local function toggle() for _, r in ipairs(remuda.keymap_snapshot().rows) do if r.action == 'session.toggle_list' then return r end end end
            assert(toggle().key == 'l')
        "#).exec().unwrap();
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn keybindings_cluster_snapshot_supports_contextual_commands_and_exit_alias() {
        let map = Snapshot::from_json(r#"{"generation":1,"ready":true,"rows":[
            {"context":"cluster.tree","key":"M-q","action":"cluster.detach","help":"detach"},
            {"context":"cluster.remote.keys","key":"C-4","action":"cluster.remote.back","help":"back"},
            {"context":"cluster.composer","key":"M-s","action":"cluster.composer.submit","help":"send"},
            {"context":"cluster.runtime","key":"C-r","action":"cluster.redraw","help":"redraw"}
        ]}"#).unwrap();
        assert_eq!(
            map.resolve(
                "cluster.tree",
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)
            ),
            Some("cluster.detach")
        );
        assert_eq!(
            map.resolve(
                "cluster.tree",
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            None
        );
        for key in ['4', '\\'] {
            assert_eq!(
                map.resolve(
                    "cluster.remote.keys",
                    KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL)
                ),
                Some("cluster.remote.back")
            );
        }
        assert!(map.help("cluster.tree").contains("M-q detach"));
        assert!(map
            .resolve(
                "cluster.search",
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)
            )
            .is_none());
        assert!(validate("cluster.tree", "z", Some("cluster.composer.submit")).is_err());
    }

    #[test]
    fn keybindings_startup_wait_is_bounded_and_accepts_only_completed_maps() {
        let mut reads = 0;
        let map = wait_for_snapshot(Duration::from_secs(1), |_| {
            reads += 1;
            Ok(Snapshot {
                ready: reads == 2,
                ..Snapshot::default()
            })
        })
        .unwrap();
        assert!(map.ready);
        assert_eq!(reads, 2);
        let error =
            wait_for_snapshot(Duration::from_millis(1), |_| Ok(Snapshot::default())).unwrap_err();
        assert!(error.contains("deadline"));
        let error = wait_for_snapshot(Duration::from_secs(1), |_| {
            Err("old daemon has no registry".into())
        })
        .unwrap_err();
        assert!(error.contains("old daemon"));
    }

    #[test]
    fn keybindings_native_factory_reads_nonblocking_config_status() {
        let lua = mlua::Lua::new();
        let table = lua.create_table().unwrap();
        let status = std::sync::Arc::new(std::sync::Mutex::new((false, None)));
        let captured = status.clone();
        install(&lua, &table, move || captured.lock().unwrap().clone()).unwrap();
        table.set("_registry", lua.create_table().unwrap()).unwrap();
        table
            .set("json", crate::json::bindings(&lua).unwrap())
            .unwrap();
        lua.globals().set("remuda", table).unwrap();
        lua.load(include_str!("tools.lua")).exec().unwrap();
        assert!(!lua
            .load("return remuda.keymap_snapshot().ready")
            .eval::<bool>()
            .unwrap());
        *status.lock().unwrap() = (true, Some("bad init.lua".into()));
        assert_eq!(
            lua.load("return remuda.keymap_snapshot().diagnostic")
                .eval::<String>()
                .unwrap(),
            "bad init.lua"
        );
        let wire = lua.load("local s=remuda.keymap_snapshot(); s.rows=remuda.json.array(s.rows); return remuda.json.encode(s)").eval::<String>().unwrap();
        let map = Snapshot::from_json(&wire).unwrap();
        assert_eq!(map.help(LIST_CONTEXT), "m message   l list");
    }

    #[test]
    fn keybindings_native_snapshot_resolves_exact_modifiers() {
        let map = Snapshot::from_json(r#"{"generation":2,"ready":true,"rows":[{"context":"session.list.browse","key":"M-l","action":"session.toggle_list","help":"list"}]}"#).unwrap();
        assert_eq!(
            map.resolve(
                "session.list.browse",
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT)
            ),
            Some("session.toggle_list")
        );
        assert_eq!(
            map.resolve(
                "session.list.browse",
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)
            ),
            None
        );
        assert_eq!(map.help("session.list.browse"), "M-l list");
    }

    #[test]
    fn keybindings_native_snapshot_rejects_bad_rows_and_duplicates() {
        for rows in [
            r#"[{"context":"session.list.browse","key":"q","action":"session.toggle_list","help":"list"}]"#,
            r#"[{"context":"session.list.browse","key":"l","action":"unknown","help":"list"}]"#,
            r#"[{"context":"session.list.browse","key":"l","action":"session.toggle_list","help":"list"},{"context":"session.list.browse","key":"l","action":"session.toggle_list","help":"list"}]"#,
        ] {
            assert!(Snapshot::from_json(&format!(
                r#"{{"generation":1,"ready":true,"rows":{rows}}}"#
            ))
            .is_err());
        }
    }

    #[test]
    fn keybindings_native_canonicalizes_aliases_and_refuses_chords() {
        assert_eq!(canonical_key("M-C-4").unwrap(), "C-M-\\");
        assert_eq!(canonical_key("<up>").unwrap(), "<up>");
        assert!(canonical_key("C-x C-f").is_err());
        assert_eq!(canonical_key("C-L").unwrap(), "C-l");
        assert_eq!(canonical_key("C-i").unwrap(), "TAB");
        assert_eq!(canonical_key("C-[").unwrap(), "ESC");
        assert!(validate(LIST_CONTEXT, "C-Q", Some(TOGGLE_LIST)).is_err());
        assert!(validate(LIST_CONTEXT, "C-m", Some(TOGGLE_LIST)).is_err());
        assert_eq!(
            canonical_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)).as_deref(),
            Some("<backtab>")
        );
    }

    #[test]
    fn keybindings_startup_poll_reports_config_failure_and_loading() {
        let loading = Snapshot::from_json(r#"{"generation":1,"ready":false,"rows":[]}"#).unwrap();
        assert!(!loading.ready);
        let failed = Snapshot::from_json(
            r#"{"generation":1,"ready":true,"diagnostic":"bad init.lua","rows":[]}"#,
        )
        .unwrap();
        assert_eq!(failed.diagnostic.as_deref(), Some("bad init.lua"));
    }
}
