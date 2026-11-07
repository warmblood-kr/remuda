//! Frontend-local cluster UI composition.
//!
//! The cluster tree and the whole-node peer list let a dedicated, restricted
//! Lua file (`cluster-ui.lua`) choose row order, row content, colors and the
//! split geometry. This module is the trust boundary: Rust hands Lua only
//! safe display facts, then validates the returned document atomically before
//! anything reaches a terminal. No daemon image is involved; composition never
//! sends an `Eval` to the local daemon or a peer.

mod geometry;
mod lua;
mod render;
mod schema;

pub use geometry::{resolve_geometry, Axis, Geometry, GeometryError, Layout};
pub use render::{render_row, render_rows};
pub use schema::{validate, ComposeError};

use serde::Serialize;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Cap on rows in one composition. A larger document is rejected, never
/// truncated: silently dropping a row could hide a navigable target.
pub const MAX_ROWS: usize = 4096;
/// Cap on spans in one row.
pub const MAX_SPANS_PER_ROW: usize = 32;
/// Cap on one span's text.
pub const MAX_SPAN_BYTES: usize = 1024;
/// Cap on spans across one composition.
pub const MAX_TOTAL_SPANS: usize = 65_536;
/// Cap on tables visited while decoding one composition.
pub const MAX_TOTAL_TABLES: usize = 32_768;
/// Cap on the serialized composition.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Cap on the user configuration source.
pub const CONFIG_MAX_BYTES: usize = 1024 * 1024;
/// Memory ceiling for the sandboxed Lua state.
pub const MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;
/// Instruction ceiling for one `compose` call. Release: ~14 ms for 1000
/// sessions; the wall clock is the backstop, not a strict latency guarantee.
pub const COMPOSE_INSTRUCTIONS: u64 = 50_000_000;
/// Instruction ceiling for loading the embedded default and user config.
pub const LOAD_INSTRUCTIONS: u64 = 20_000_000;
/// Wall-clock backstop for one `compose` call.
pub const COMPOSE_WALL_MS: u64 = 100;
/// Wall backstop for the large-fixture test seam only. Debug and instrumented
/// CI builds need more than the shipped envelope for a 1000-session fixture.
const FIXTURE_WALL_MS: u64 = 2_000;
/// Wall-clock backstop for loading one source.
pub const LOAD_WALL_MS: u64 = 250;
/// How long a failed composition keeps using the fallback before retrying.
pub const FAILURE_BACKOFF_MS: u64 = 10_000;

/// Which surface is being composed. Both share the same schema so a config can
/// style both, but the facts differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Cluster,
    Peer,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Cluster => "cluster",
            Surface::Peer => "peer",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    Local,
    Node,
    Peer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Reachable,
    Stale,
    Reconnecting,
    Unreachable,
}

impl NodeState {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeState::Reachable => "reachable",
            NodeState::Stale => "stale",
            NodeState::Reconnecting => "reconnecting",
            NodeState::Unreachable => "unreachable",
        }
    }
}

/// Geometry the caller allows the list to claim. `body_rows` excludes chrome;
/// `preview_chrome_rows` is the native header inside the preview leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Viewport {
    pub cols: u16,
    pub rows: u16,
    pub body_rows: u16,
    pub gutter_cells: u16,
    pub preview_chrome_rows: u16,
}

impl Viewport {
    pub fn new(cols: u16, rows: u16, body_rows: u16) -> Self {
        Self {
            cols,
            rows,
            body_rows,
            gutter_cells: 1,
            preview_chrome_rows: 1,
        }
    }

    pub fn with_preview_chrome(mut self, rows: u16) -> Self {
        self.preview_chrome_rows = rows;
        self
    }

    pub fn with_gutter(mut self, cells: u16) -> Self {
        self.gutter_cells = cells;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub reachable: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionEntry {
    pub id: u32,
    pub label: String,
    pub selected: bool,
    pub alive: bool,
    pub pending: usize,
    /// Display width of the status Rust appends after this row.
    pub status_cells: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Group {
    pub id: u32,
    pub kind: GroupKind,
    pub selected: bool,
    pub expanded: bool,
    pub label: String,
    pub state: Option<NodeState>,
    pub age_seconds: Option<u64>,
    pub pending: usize,
    pub status_cells: u16,
    pub sessions: Vec<SessionEntry>,
}

/// Facts a composition may see. Tokens are per-call integers; the caller keeps
/// the routing map privately, so a document can never name a wire session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Model {
    pub version: u32,
    pub surface: Surface,
    pub viewport: Viewport,
    pub selected: Option<u32>,
    pub counts: Option<Counts>,
    pub searching: bool,
    pub attention_only: bool,
    pub all_empty: bool,
    /// Mouse-drag preference from the user, in list cells. The default honors
    /// it when it fits; a document may ignore it.
    pub preferred_list_cells: Option<u16>,
    pub groups: Vec<Group>,
    /// Canonical status text per operational target, keyed by token. It is
    /// display-only (already sanitized and bounded by the caller) and is never
    /// read from Lua; a `field="status"` span is replaced with this text.
    pub statuses: Vec<(u32, String)>,
}

impl Model {
    pub fn new(surface: Surface, viewport: Viewport) -> Self {
        Self {
            version: 1,
            surface,
            viewport,
            selected: None,
            counts: None,
            searching: false,
            attention_only: false,
            all_empty: false,
            preferred_list_cells: None,
            groups: Vec::new(),
            statuses: Vec::new(),
        }
    }

    /// Canonical status text for a target, falling back to semantic facts when
    /// the caller did not install the exact display text.
    pub fn status_for(&self, id: u32) -> Option<String> {
        if let Some((_, text)) = self.statuses.iter().find(|(token, _)| *token == id) {
            return Some(text.clone());
        }
        for group in &self.groups {
            if group.id == id {
                return match group.kind {
                    GroupKind::Node => Some(format!("· {}", group.state?.as_str())),
                    GroupKind::Local | GroupKind::Peer => None,
                };
            }
            if let Some(session) = group.sessions.iter().find(|session| session.id == id) {
                let base = if session.alive { "live" } else { "ended" };
                return Some(if session.pending > 0 {
                    format!("{base} · pending input {}", session.pending)
                } else {
                    base.to_string()
                });
            }
        }
        None
    }

    pub fn fingerprint(&self) -> String {
        // The model is small and entirely printable; an exact JSON key avoids
        // hash collisions choosing a stale row/target set.
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn group(&self, id: u32) -> Option<&Group> {
        self.groups.iter().find(|group| group.id == id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Color {
    Index(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            fg: None,
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }

    pub fn styled(mut self, color: Option<Color>, bold: bool, reverse: bool) -> Self {
        self.fg = color;
        self.bold = bold;
        self.reverse = reverse;
        self
    }
}

/// One validated row. `target` is a private token the caller maps to a route;
/// `status_id` may name a group whose status must be appended even though the
/// row itself is not navigable (the local/peer group).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub spans: Vec<Span>,
    pub target: Option<u32>,
    pub selected: bool,
    pub gutter: bool,
    /// Index of the canonical status span. The renderer reserves its width
    /// before clipping the rest of the row.
    pub status: Option<usize>,
}

impl Row {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            spans: vec![Span::plain(text)],
            target: None,
            selected: false,
            gutter: false,
            status: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Composition {
    pub title: Vec<Row>,
    pub groups: Vec<ComposedGroup>,
    pub empty: Vec<Row>,
    pub layout: Geometry,
    /// Set when this document came from the built-in default because the user
    /// configuration failed; bounded and safe for a footer line.
    pub diagnostic: Option<String>,
    pub source: ComposeSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposedGroup {
    pub id: u32,
    pub kind: GroupKind,
    pub rows: Vec<Row>,
    pub sessions: Vec<ComposedSession>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposedSession {
    pub id: u32,
    pub rows: Vec<Row>,
}

impl Composition {
    /// Rows in render order. Empty-state hints follow the local group, as the
    /// old tree did, instead of appearing after remote nodes.
    pub fn flattened_rows(&self) -> Vec<Row> {
        let mut rows = self.title.clone();
        let mut placed_empty = false;
        for group in &self.groups {
            rows.extend(group.rows.iter().cloned());
            for session in &group.sessions {
                rows.extend(session.rows.iter().cloned());
            }
            if !placed_empty && group.kind == GroupKind::Local {
                rows.extend(self.empty.iter().cloned());
                placed_empty = true;
            }
        }
        if !placed_empty {
            rows.extend(self.empty.iter().cloned());
        }
        rows
    }

    /// Every navigable token in render order, exactly once.
    pub fn targets(&self) -> Vec<u32> {
        let mut out = Vec::new();
        for group in &self.groups {
            for row in &group.rows {
                if let Some(target) = row.target {
                    out.push(target);
                }
            }
            for session in &group.sessions {
                for row in &session.rows {
                    if let Some(target) = row.target {
                        out.push(target);
                    }
                }
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeSource {
    User,
    Default,
    /// The caller must draw its emergency roster.
    Emergency,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposeFailure {
    pub message: String,
    pub source: ComposeSource,
}

impl std::fmt::Display for ComposeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A sandboxed Lua composer plus its fallback default. The user document is
/// loaded once; a runtime/budget/schema failure uses the embedded default, and
/// a default failure leaves the caller's emergency roster.
pub struct Composer {
    inner: RefCell<Inner>,
    default_source: String,
    load_diagnostic: Option<String>,
}

struct Inner {
    user: Option<lua::Vm>,
    default: Option<lua::Vm>,
    default_failed: bool,
    user_failed: bool,
    retry_at: Option<Instant>,
    cached: Option<(String, Composition)>,
}

impl Composer {
    /// Load the embedded default and the Config/cluster-ui.lua user file.
    pub fn system() -> Self {
        let config = config_path().and_then(|path| match read_bounded(&path) {
            Ok(Some(source)) => Some((path, Ok(source))),
            Ok(None) => None,
            Err(error) => Some((path, Err(error))),
        });
        Self::build(DEFAULT_SOURCE.to_string(), config)
    }

    /// The built-in composition with no user document.
    pub fn embedded() -> Self {
        Self::build(DEFAULT_SOURCE.to_string(), None)
    }

    /// Test seam: inject both sources. `None` config means default only.
    pub fn with_sources(embedded: &str, config: Option<(&str, &str)>) -> Self {
        let config = config.map(|(name, source)| {
            (
                PathBuf::from(name),
                Ok::<String, String>(source.to_string()),
            )
        });
        Self::build(embedded.to_string(), config)
    }

    fn build(default_source: String, config: Option<(PathBuf, Result<String, String>)>) -> Self {
        let mut load_diagnostic = None;
        // The embedded default is part of the binary; a failure is a build
        // bug. The caller still gets its emergency roster.
        let default = lua::Vm::load(
            &[("embedded cluster_ui.lua", default_source.as_str())],
            LOAD_INSTRUCTIONS,
            LOAD_WALL_MS,
        )
        .ok();
        let mut user = None;
        if let Some((path, result)) = config {
            match (default.as_ref(), result) {
                (Some(previous), Ok(source)) => {
                    let name = path.display().to_string();
                    let sources = [
                        ("embedded cluster_ui.lua", default_source.as_str()),
                        (name.as_str(), source.as_str()),
                    ];
                    match previous.reload_with(&sources) {
                        Ok(vm) => user = Some(vm),
                        Err(error) => {
                            load_diagnostic = Some(config_diagnostic(&path, &error));
                        }
                    }
                }
                (_, Err(error)) => {
                    load_diagnostic = Some(config_diagnostic(&path, &error));
                }
                (None, _) => {}
            }
        }
        Self {
            inner: RefCell::new(Inner {
                user,
                default,
                default_failed: false,
                user_failed: false,
                retry_at: None,
                cached: None,
            }),
            default_source,
            load_diagnostic,
        }
    }

    /// Bounded diagnostic for the footer: configuration load failure or the
    /// last composition fallback. Never contains peer endpoints.
    pub fn diagnostic(&self) -> Option<String> {
        let inner = self.inner.borrow();
        if inner.retry_at.is_some() || inner.user_failed {
            return Some("cluster ui: using the built-in composition".to_string());
        }
        self.load_diagnostic.clone()
    }

    /// Compose, or report that the caller must use its emergency roster.
    pub fn compose(&self, model: &Model) -> Result<Composition, ComposeFailure> {
        self.compose_with_wall(model, COMPOSE_WALL_MS)
    }

    /// Test-only seam for the 1000-session structural fixtures. It keeps every
    /// shipped cap and uses a wider wall backstop because debug and instrumented
    /// builds are slower; no environment or user configuration reaches it.
    #[doc(hidden)]
    pub fn compose_fixture(&self, model: &Model) -> Result<Composition, ComposeFailure> {
        self.compose_with_wall(model, FIXTURE_WALL_MS)
    }

    fn compose_with_wall(
        &self,
        model: &Model,
        wall_ms: u64,
    ) -> Result<Composition, ComposeFailure> {
        let key = model.fingerprint();
        if let Some(composition) = self.cached(&key) {
            return Ok(composition);
        }
        if let Some(retry_at) = self.inner.borrow().retry_at {
            if Instant::now() < retry_at {
                return Err(ComposeFailure {
                    message: "cluster ui composition is backing off".into(),
                    source: ComposeSource::Emergency,
                });
            }
        }

        let mut inner = self.inner.borrow_mut();
        if let Some(composition) = inner
            .cached
            .as_ref()
            .filter(|(cached_key, _)| cached_key == &key)
            .map(|(_, composition)| composition.clone())
        {
            return Ok(composition);
        }

        if let Some(vm) = inner.user.as_mut() {
            match compose_with(vm, model, wall_ms) {
                Ok(mut composition) => {
                    composition.source = ComposeSource::User;
                    composition.diagnostic = self.load_diagnostic.clone();
                    inner.cached = Some((key, composition.clone()));
                    inner.retry_at = None;
                    return Ok(composition);
                }
                Err(error) => {
                    inner.user_failed = true;
                    inner.user = None;
                    inner.cached = None;
                    let diagnostic = format!("cluster ui config: {}", bounded(&error));
                    return self
                        .default_composition(&mut inner, model, key, Some(diagnostic), wall_ms)
                        .map_err(|()| ComposeFailure {
                            message: "cluster ui composition failed; emergency roster".into(),
                            source: ComposeSource::Emergency,
                        });
                }
            }
        }

        let composition = self.default_composition(&mut inner, model, key, None, wall_ms);
        match composition {
            Ok(composition) => Ok(composition),
            Err(_) => Err(ComposeFailure {
                message: "cluster ui composition failed; emergency roster".into(),
                source: ComposeSource::Emergency,
            }),
        }
    }

    fn cached(&self, key: &str) -> Option<Composition> {
        self.inner
            .borrow()
            .cached
            .as_ref()
            .filter(|(cached_key, _)| cached_key == key)
            .map(|(_, composition)| composition.clone())
    }

    fn default_composition(
        &self,
        inner: &mut Inner,
        model: &Model,
        key: String,
        diagnostic: Option<String>,
        wall_ms: u64,
    ) -> Result<Composition, ()> {
        let default = if inner.default.is_none() || inner.default_failed {
            lua::Vm::load(
                &[("embedded cluster_ui.lua", self.default_source.as_str())],
                LOAD_INSTRUCTIONS,
                LOAD_WALL_MS,
            )
            .ok()
        } else {
            inner.default.take()
        };
        let Some(mut default) = default else {
            inner.default_failed = true;
            inner.retry_at = Some(Instant::now() + backoff());
            return Err(());
        };
        let result = compose_with(&mut default, model, wall_ms);
        match result {
            Ok(mut composition) => {
                composition.source = ComposeSource::Default;
                composition.diagnostic = diagnostic.or_else(|| self.load_diagnostic.clone());
                inner.default = Some(default);
                inner.default_failed = false;
                inner.cached = Some((key, composition.clone()));
                inner.retry_at = None;
                Ok(composition)
            }
            Err(_) => {
                inner.default = Some(default);
                inner.default_failed = true;
                inner.retry_at = Some(Instant::now() + backoff());
                inner.cached = None;
                Err(())
            }
        }
    }
}

fn compose_with(vm: &mut lua::Vm, model: &Model, wall_ms: u64) -> Result<Composition, String> {
    let raw = vm
        .compose(model, COMPOSE_INSTRUCTIONS, wall_ms)
        .map_err(|error| bounded(&error))?;
    if raw.encoded_len > MAX_OUTPUT_BYTES {
        return Err("composition output is larger than 1 MiB".to_string());
    }
    validate(model, raw).map_err(|error| error.to_string())
}

fn config_diagnostic(path: &std::path::Path, error: &str) -> String {
    bounded(&format!("cluster ui config {}: {error}", path.display()))
}

fn bounded(text: &str) -> String {
    let safe = crate::text::strip_terminal_controls(text);
    let mut out: String = safe.chars().take(200).collect();
    if safe.chars().count() > 200 {
        out.push('…');
    }
    out
}

fn backoff() -> Duration {
    Duration::from_millis(FAILURE_BACKOFF_MS)
}

const DEFAULT_SOURCE: &str = include_str!("cluster_ui.lua");

/// The embedded default source, for tests and callers that need to layer a
/// user document on top of it.
pub fn default_source() -> &'static str {
    DEFAULT_SOURCE
}

fn read_bounded(path: &std::path::Path) -> Result<Option<String>, String> {
    use std::io::Read;
    // Check the path first so an accidental FIFO is rejected without opening
    // it. The handle check below closes the race where the path is replaced
    // between the two steps; on unix the open is non-blocking so a FIFO that
    // wins the race still cannot stall startup.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect: {error}")),
    };
    if !metadata.is_file() {
        return Err("not a regular file".to_string());
    }
    let mut file = open_config(path)?;
    if !file
        .metadata()
        .map_err(|error| format!("cannot inspect: {error}"))?
        .is_file()
    {
        return Err("not a regular file".to_string());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(u64::try_from(CONFIG_MAX_BYTES + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read: {error}"))?;
    if bytes.len() > CONFIG_MAX_BYTES {
        return Err("file is larger than 1 MiB".to_string());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "file is not UTF-8".to_string())
}

#[cfg(unix)]
fn open_config(path: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot read: {error}"))
}

#[cfg(not(unix))]
fn open_config(path: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::File::open(path).map_err(|error| format!("cannot read: {error}"))
}

fn config_path() -> Option<PathBuf> {
    crate::storage::user_file_path_for(
        crate::storage::Kind::Config,
        "cluster-ui.lua",
        cfg!(windows),
        &|name| std::env::var_os(name),
    )
}

/// Display width in terminal cells, the same measure the renderer uses.
pub fn text_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(text)
}

/// Fit `text` into `cells` display columns, adding `…` when it must cut.
pub fn fit_text(text: &str, cells: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if UnicodeWidthStr::width(text) <= cells {
        return text.to_string();
    }
    if cells == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width + 1 > cells {
            break;
        }
        out.push_str(grapheme);
        used += width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Viewport {
        Viewport::new(100, 24, 22).with_preview_chrome(1)
    }

    fn model() -> Model {
        let mut model = Model::new(Surface::Cluster, viewport());
        model.counts = Some(Counts {
            reachable: 1,
            total: 1,
        });
        model.selected = Some(2);
        model.groups.push(Group {
            id: 1,
            kind: GroupKind::Local,
            selected: false,
            expanded: true,
            label: "studio".into(),
            state: None,
            age_seconds: Some(0),
            pending: 0,
            status_cells: 27,
            sessions: vec![SessionEntry {
                id: 2,
                label: "dev".into(),
                selected: true,
                alive: true,
                pending: 0,
                status_cells: 4,
            }],
        });
        model
    }

    #[test]
    fn the_embedded_default_composes_a_local_group_and_session() {
        let composer = Composer::with_sources(DEFAULT_SOURCE, None);
        let composition = composer.compose(&model()).expect("default composes");
        assert_eq!(composition.groups.len(), 1);
        assert_eq!(composition.groups[0].sessions.len(), 1);
        assert_eq!(
            composition.targets(),
            vec![2],
            "only the session is navigable"
        );
        assert_eq!(composition.layout.axis, geometry::Axis::Horizontal);
        assert!(composition.layout.list_cells >= 24);
    }

    #[test]
    fn the_sandbox_removes_io_and_dynamic_loading() {
        let composer = Composer::with_sources("function compose(model) os.exit(1) end", None);
        assert!(composer.compose(&model()).is_err());
    }

    #[test]
    fn a_thousand_session_fixture_composes_without_losing_targets() {
        let mut model = model();
        let sessions = model.groups[0].sessions[0].clone();
        model.groups[0].sessions.clear();
        for index in 0..1000u32 {
            let mut session = sessions.clone();
            session.id = index + 2;
            session.label = format!("会话{index}");
            session.selected = index == 0;
            model.groups[0].sessions.push(session);
        }
        model.selected = Some(2);
        model.statuses.clear();
        for session in &model.groups[0].sessions {
            model.statuses.push((session.id, "live".to_string()));
        }
        let load_started = std::time::Instant::now();
        let composer = Composer::embedded();
        let load = load_started.elapsed();
        let started = std::time::Instant::now();
        let composition = composer.compose_fixture(&model).expect("fixture composes");
        eprintln!(
            "1000-session load {load:?}, compose {:?}",
            started.elapsed()
        );
        assert_eq!(composition.targets().len(), 1000);
    }

    #[test]
    fn the_pattern_engine_is_not_reachable() {
        let composer = Composer::with_sources(
            DEFAULT_SOURCE,
            Some((
                "cluster-ui.lua",
                r#"
                local base = compose
                function compose(model)
                  string.find(string.rep("a", 30), string.rep("a?", 30) .. string.rep("a", 30) .. "b")
                  return base(model)
                end
                "#,
            )),
        );
        let started = std::time::Instant::now();
        let composition = composer.compose(&model()).expect("fallback composes");
        assert_eq!(composition.source, ComposeSource::Default);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn a_metatable_on_the_result_is_rejected() {
        let composer = Composer::with_sources(
            DEFAULT_SOURCE,
            Some((
                "cluster-ui.lua",
                r#"
                local base = compose
                function compose(model)
                  local result = base(model)
                  return setmetatable(result, { __index = function() error("nope") end })
                end
                "#,
            )),
        );
        let composition = composer.compose(&model()).expect("fallback composes");
        assert_eq!(composition.source, ComposeSource::Default);
    }

    #[test]
    fn pcall_cannot_catch_the_budget() {
        let composer = Composer::with_sources(
            DEFAULT_SOURCE,
            Some((
                "cluster-ui.lua",
                r#"
                local base = compose
                function compose(model)
                  pcall(function() while true do end end)
                  return base(model)
                end
                "#,
            )),
        );
        let composition = composer.compose(&model()).expect("fallback composes");
        assert_eq!(composition.source, ComposeSource::Default);
    }

    #[test]
    fn an_infinite_loop_falls_back_without_hanging() {
        let composer = Composer::with_sources(
            DEFAULT_SOURCE,
            Some((
                "cluster-ui.lua",
                "function compose(model) while true do end end",
            )),
        );
        let started = std::time::Instant::now();
        let composition = composer.compose(&model()).expect("fallback composes");
        assert_eq!(composition.source, ComposeSource::Default);
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[test]
    fn a_user_source_can_replace_compose() {
        let composer = Composer::with_sources(
            DEFAULT_SOURCE,
            Some((
                "cluster-ui.lua",
                r#"
                function compose(model)
                  return {
                    version = 1,
                    title = {},
                    groups = {
                      { id = 1, rows = {}, sessions = {
                        { id = 2, rows = { { spans = { { text = "custom" }, { field = "status" } } } } },
                      } },
                    },
                    empty = {},
                    layout = { axis = "vertical", list_cells = 1 },
                  }
                end
                "#,
            )),
        );
        let composition = composer.compose(&model()).expect("user composes");
        assert_eq!(
            composition.source,
            ComposeSource::User,
            "{:?}",
            composition.diagnostic
        );
        assert_eq!(composition.layout.axis, geometry::Axis::Vertical);
        assert_eq!(
            composition.groups[0].sessions[0].rows[0].spans[0].text,
            "custom"
        );
    }
}
