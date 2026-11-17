use crate::{discover_peers, peer_is_unique, worker_deadline, Selection};
use remuda_core::agent::{Color, StyledCell};
use remuda_core::protocol::StyledRun;
use remuda_native::session_preview::{capture, IdentityPolicy, PreviewFrame};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct SyncFrame {
    pub text: String,
    /// Structured cells keep the attributes the plain string cannot carry.
    #[serde(default)]
    pub cells: Vec<Vec<StyledCell>>,
    pub version: u64,
    pub instance: String,
}

/// The first, non-attach preview: safe text plus the same cells with styles.
#[derive(Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct PreviewPayload {
    pub text: String,
    pub cells: Vec<Vec<StyledCell>>,
}

const BOLD: u8 = 1;
const DIM: u8 = 2;
const ITALIC: u8 = 4;
const UNDERLINE: u8 = 8;
const INVERSE: u8 = 16;
const WIDE: u8 = 32;

fn color_is_default(color: &Color) -> bool {
    *color == Color::Default
}

fn flags_are_zero(flags: &u8) -> bool {
    *flags == 0
}

/// ABI 2 sends one compact run per style group instead of one object per cell.
/// `StyledRun`'s field names are wire-stable for Sync between peers; this shape
/// is local to the GUI worker and can stay short.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRun {
    pub t: String,
    #[serde(default, skip_serializing_if = "color_is_default")]
    pub f: Color,
    #[serde(default, skip_serializing_if = "color_is_default")]
    pub b: Color,
    #[serde(default, skip_serializing_if = "flags_are_zero")]
    pub s: u8,
}

impl WireRun {
    fn from_run(run: &StyledRun) -> Self {
        let mut flags = 0;
        if run.bold {
            flags |= BOLD;
        }
        if run.dim {
            flags |= DIM;
        }
        if run.italic {
            flags |= ITALIC;
        }
        if run.underline {
            flags |= UNDERLINE;
        }
        if run.inverse {
            flags |= INVERSE;
        }
        if run.wide {
            flags |= WIDE;
        }
        Self {
            t: run.text.clone(),
            f: run.fg,
            b: run.bg,
            s: flags,
        }
    }
    fn to_run(&self) -> StyledRun {
        StyledRun {
            text: self.t.clone(),
            fg: self.f,
            bg: self.b,
            bold: self.s & BOLD != 0,
            dim: self.s & DIM != 0,
            italic: self.s & ITALIC != 0,
            underline: self.s & UNDERLINE != 0,
            inverse: self.s & INVERSE != 0,
            wide: self.s & WIDE != 0,
        }
    }
}

/// ABI 2's preview reply: text plus runs, never cells.
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePreview {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Vec<WireRun>>,
}

fn is_zero_width(c: char) -> bool {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(0) == 0
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// Characters that extend the preceding cluster instead of starting a cell.
fn extends_previous(c: char) -> bool {
    is_zero_width(c)
        || ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
        || ('\u{E0020}'..='\u{E007F}').contains(&c)
}

/// ABI 2's opt-in decoder: one cell per grapheme cluster, so combining marks,
/// ZWJ sequences, variation selectors and emoji modifiers stay with their base.
/// The shared v1 `expand_runs` keeps its historical one-cell-per-scalar shape.
fn expand_clusters(runs: &[StyledRun]) -> Vec<StyledCell> {
    fn cell_of(run: &StyledRun, text: String) -> StyledCell {
        StyledCell {
            text,
            fg: run.fg,
            bg: run.bg,
            bold: run.bold,
            dim: run.dim,
            italic: run.italic,
            underline: run.underline,
            inverse: run.inverse,
            wide: run.wide,
        }
    }
    let mut cells = Vec::new();
    for run in runs {
        let mut cluster = String::new();
        let mut regional = 0usize;
        let mut after_joiner = false;
        for c in run.text.chars() {
            let joins = !cluster.is_empty()
                && (extends_previous(c)
                    || after_joiner
                    || (is_regional_indicator(c)
                        && regional % 2 == 1
                        && cluster.chars().last().is_some_and(is_regional_indicator)));
            if !joins && !cluster.is_empty() {
                cells.push(cell_of(run, std::mem::take(&mut cluster)));
            }
            cluster.push(c);
            regional = if is_regional_indicator(c) {
                regional + 1
            } else {
                0
            };
            after_joiner = if c == '\u{200D}' {
                true
            } else {
                after_joiner && is_zero_width(c)
            };
        }
        if !cluster.is_empty() {
            cells.push(cell_of(run, cluster));
        }
    }
    cells
}

impl WirePreview {
    pub fn from_payload(payload: &PreviewPayload) -> Self {
        Self {
            text: payload.text.clone(),
            rows: payload
                .cells
                .iter()
                .map(|row| {
                    remuda_core::protocol::collapse_runs(row)
                        .iter()
                        .map(WireRun::from_run)
                        .collect()
                })
                .collect(),
        }
    }
    pub fn into_payload(self) -> PreviewPayload {
        PreviewPayload {
            text: self.text,
            cells: self
                .rows
                .iter()
                .map(|row| {
                    let runs: Vec<StyledRun> = row.iter().map(WireRun::to_run).collect();
                    expand_clusters(&runs)
                })
                .collect(),
        }
    }
    /// Text-only form used when the structured runs would not fit the worker's
    /// bounded stdout. The caller still gets an honest plain preview.
    fn text_only(mut self) -> Self {
        self.rows.clear();
        self
    }
}

/// ABI 2's Sync reply. The v1 shape is the same minus `rows`, so an older
/// parent still reads it with serde defaults.
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSync {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Vec<WireRun>>,
    pub version: u64,
    pub instance: String,
}

impl WireSync {
    pub fn from_frame(frame: &SyncFrame) -> Self {
        let wire = WirePreview::from_payload(&PreviewPayload {
            text: frame.text.clone(),
            cells: frame.cells.clone(),
        });
        Self {
            text: wire.text,
            rows: wire.rows,
            version: frame.version,
            instance: frame.instance.clone(),
        }
    }
    pub fn into_frame(self) -> SyncFrame {
        let payload = WirePreview {
            text: self.text,
            rows: self.rows,
        }
        .into_payload();
        SyncFrame {
            text: payload.text,
            cells: payload.cells,
            version: self.version,
            instance: self.instance,
        }
    }
}

fn serialized_len<T: Serialize>(value: &T, abi: &str) -> Option<usize> {
    serde_json::to_vec(&crate::WorkerReply {
        abi: abi.to_owned(),
        payload: value,
    })
    .ok()
    .map(|bytes| bytes.len())
}

/// Keep the structured reply inside the worker's stdout cap with margin for
/// the envelope. Falls back to text only rather than raising the cap.
fn encode_preview(payload: &PreviewPayload) -> WirePreview {
    let wire = WirePreview::from_payload(payload);
    if serialized_len(&wire, crate::WORKER_ABI_2).is_some_and(|len| len <= crate::WIRE_BUDGET) {
        wire
    } else {
        wire.text_only()
    }
}

fn encode_sync(frame: &SyncFrame) -> WireSync {
    let wire = WireSync::from_frame(frame);
    if serialized_len(&wire, crate::WORKER_ABI_2).is_some_and(|len| len <= crate::WIRE_BUDGET) {
        wire
    } else {
        WireSync {
            text: wire.text,
            rows: Vec::new(),
            version: wire.version,
            instance: wire.instance,
        }
    }
}

/// Read a preview reply from either new (ABI 2, structured), v1 (tagged
/// string) or legacy (untagged string) worker output. The string shapes remain
/// valid: they render as plain text, never as a silent failure.
pub fn read_preview(bytes: &[u8]) -> Result<PreviewPayload, String> {
    if let Ok(reply) = serde_json::from_slice::<crate::WorkerReply<WirePreview>>(bytes) {
        if reply.abi == crate::WORKER_ABI_2 {
            return Ok(reply.payload.into_payload());
        }
    }
    if let Ok(reply) = serde_json::from_slice::<crate::WorkerReply<String>>(bytes) {
        if reply.abi == crate::WORKER_ABI {
            return Ok(PreviewPayload {
                text: reply.payload,
                cells: Vec::new(),
            });
        }
    }
    if let Ok(text) = serde_json::from_slice::<String>(bytes) {
        return Ok(PreviewPayload {
            text,
            cells: Vec::new(),
        });
    }
    Err(crate::STALE_HELPER.to_string())
}

pub fn read_sync(bytes: &[u8]) -> Result<SyncFrame, String> {
    if let Ok(reply) = serde_json::from_slice::<crate::WorkerReply<WireSync>>(bytes) {
        if reply.abi == crate::WORKER_ABI_2 {
            return Ok(reply.payload.into_frame());
        }
    }
    if let Ok(reply) = serde_json::from_slice::<crate::WorkerReply<SyncFrame>>(bytes) {
        if reply.abi == crate::WORKER_ABI {
            return Ok(reply.payload);
        }
    }
    if let Ok(frame) = serde_json::from_slice::<SyncFrame>(bytes) {
        return Ok(frame);
    }
    Err(crate::STALE_HELPER.to_string())
}


/// Ask the worker for ABI 2 first and fall back to the read-only ABI 1 shape
/// for that request if ABI 2 fails. Retry ABI 2 on the next read: an older
/// worker and a transient silent failure cannot reliably be distinguished.
/// No input is ever resent.
pub fn request_preview(
    path: &Path,
    selection: &Selection,
) -> Result<PreviewPayload, String> {
    let peer = selection.peer.as_deref().unwrap_or("");
    let kind = if selection.peer.is_some() {
        "peer"
    } else {
        "local"
    };
    let attempt = |tag: &str| {
        crate::provider::output(
            path,
            &[
                "--internal-preview-worker",
                kind,
                peer,
                &selection.name,
                &selection.instance,
                tag,
            ],
            crate::provider::TIMEOUT,
        )
        .and_then(|bytes| read_preview(&bytes))
    };
    match attempt(crate::WORKER_ABI_2) {
        Ok(payload) => Ok(payload),
        Err(_) => attempt(crate::WORKER_ABI),
    }
}

/// The Sync sibling of [`request_preview`]; ABI 1 is text-only and read-only.
pub fn request_sync(
    path: &Path,
    selection: &Selection,
    since: u64,
) -> Result<SyncFrame, String> {
    let peer = selection.peer.as_deref().unwrap_or("");
    let kind = if selection.peer.is_some() {
        "peer"
    } else {
        "local"
    };
    let since = since.to_string();
    let attempt = |tag: &str| {
        crate::provider::output(
            path,
            &[
                "--internal-preview-worker",
                kind,
                peer,
                &selection.name,
                &selection.instance,
                "--sync",
                &since,
                tag,
            ],
            crate::provider::TIMEOUT,
        )
        .and_then(|bytes| read_sync(&bytes))
    };
    match attempt(crate::WORKER_ABI_2) {
        Ok(frame) => Ok(frame),
        Err(_) => attempt(crate::WORKER_ABI),
    }
}

/// Positions of tag characters that form a complete subdivision-flag sequence
/// (black flag, tag run, cancel tag). Other tag characters stay filtered.
fn tagged_positions(chars: &[char]) -> Vec<bool> {
    let mut tagged = vec![false; chars.len()];
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\u{1F3F4}' {
            let mut end = i + 1;
            while end < chars.len() && ('\u{E0020}'..='\u{E007E}').contains(&chars[end]) {
                end += 1;
            }
            if end > i + 1 && end < chars.len() && chars[end] == '\u{E007F}' {
                for slot in tagged.iter_mut().take(end + 1).skip(i + 1) {
                    *slot = true;
                }
                i = end;
            }
        }
        i += 1;
    }
    tagged
}

/// A legitimate ZWJ or a validated tag sequence belongs to its cluster; every
/// other control or bidi/format character is still dropped.
fn safe_cell_char(ch: char, tagged: bool) -> bool {
    if ch.is_control() {
        return false;
    }
    if ch == '\u{200D}' || tagged {
        return true;
    }
    !remuda_core::protocol::is_secret_prompt_format_or_separator(ch)
}

/// Keep the cell grid and its attributes while applying control, bidi and
/// secret-prompt filtering. A cell whose text was entirely filtered keeps a
/// blank column, so a later collapse cannot mistake it for a wide continuation.
fn structured(frame: PreviewFrame) -> Result<PreviewPayload, String> {
    let mut text = String::new();
    let mut rows = Vec::new();
    for row in frame.cells {
        // Strict decode expands a run to one scalar cell, so a tag sequence can
        // span cells. Compute its validity across the whole row before
        // sanitizing each cell.
        let row_chars: Vec<char> = row.iter().flat_map(|cell| cell.text.chars()).collect();
        let tagged = tagged_positions(&row_chars);
        let mut offset = 0usize;
        let mut safe_row = Vec::with_capacity(row.len());
        for cell in row {
            let mut safe = String::new();
            for ch in cell.text.chars() {
                if safe_cell_char(ch, tagged.get(offset).copied().unwrap_or(false)) {
                    safe.push(ch);
                }
                offset += 1;
            }
            // Strict decode empties a control/bidi-only cell before this stage,
            // and it never emits wide continuation cells. Every empty cell is
            // therefore one blank column; it must not stay empty, or a collapse
            // would treat it as a wide continuation and shift what follows.
            if safe.is_empty() {
                safe.push(' ');
            }
            for ch in safe.chars() {
                if ch.len_utf8() > 65_536usize.saturating_sub(text.len()) {
                    return Err("too large".into());
                }
                text.push(ch);
            }
            if text.len() == 65_536 {
                return Err("too large".into());
            }
            let mut cell = cell;
            cell.text = safe;
            safe_row.push(cell);
        }
        if text.len() >= 65_536 {
            return Err("too large".into());
        }
        text.push('\n');
        rows.push(safe_row);
    }
    Ok(PreviewPayload { text, cells: rows })
}

#[cfg(test)]
fn plain(frame: PreviewFrame) -> Result<String, String> {
    structured(frame).map(|payload| payload.text)
}

fn fetch(s: &Selection) -> Result<PreviewPayload, String> {
    let result = match &s.peer {
        None => capture(
            remuda_native::daemon::socket_path("default").as_path(),
            &s.name,
            Some(&s.instance),
            0,
            IdentityPolicy::Strict,
        )
        .and_then(structured),
        Some(fp) => {
            if !peer_is_unique(&discover_peers().map_err(|e| e.to_string())?, fp) {
                return Err("peer unavailable".into());
            }
            match remuda_native::cluster_api::resolve_call(fp, None).map_err(|e| e.to_string())? {
                remuda_native::cluster_api::ResolvedCall::Remote { fingerprint, .. }
                    if fingerprint == *fp => {}
                _ => return Err("peer unavailable".into()),
            }
            let port = remuda_native::tui::RemotePort::system(fp).map_err(|e| e.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(4);
            let text = loop {
                match capture(&port, &s.name, Some(&s.instance), 0, IdentityPolicy::Strict)
                    .and_then(structured)
                {
                    Ok(text) => break Ok(text),
                    Err(e) if Instant::now() >= deadline => break Err(e),
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            };
            if !peer_is_unique(&discover_peers().map_err(|e| e.to_string())?, fp) {
                return Err("peer unavailable".into());
            }
            text
        }
    };
    result
}

/// ABI 2 asks for structured runs; ABI 1 and the untagged legacy form keep the
/// string reply they were built against.
pub fn worker(args: &[String], abi: Option<u8>) -> i32 {
    if args.len() != 4 || args[2].is_empty() || args[3].is_empty() {
        return 1;
    }
    let peer = match args[0].as_str() {
        "local" if args[1].is_empty() => None,
        "peer" if !args[1].is_empty() => Some(args[1].clone()),
        _ => return 1,
    };
    worker_deadline();
    let s = Selection {
        peer,
        name: args[2].clone(),
        instance: args[3].clone(),
    };
    match fetch(&s) {
        Ok(payload) => match abi {
            // ABI 1's payload is a JSON string, not the v0 struct.
            Some(1) => crate::write_worker_reply_abi(&payload.text, Some(crate::WORKER_ABI)),
            Some(2) => crate::write_worker_reply_abi(
                &encode_preview(&payload),
                Some(crate::WORKER_ABI_2),
            ),
            _ => crate::write_worker_reply_abi(&payload.text, None),
        },
        Err(_) => 1,
    }
}

/// A separate bounded worker keeps daemon waits and decoding off the GUI thread.
/// The existing preview command rejects these extended arguments on older builds.
pub fn sync_worker(args: &[String], abi2: bool) -> i32 {
    if args.len() != 6
        || !((args[0] == "local" && args[1].is_empty())
            || (args[0] == "peer" && !args[1].is_empty()))
        || args[2].is_empty()
        || args[3].is_empty()
        || args[4] != "--sync"
    {
        return 1;
    }
    let Ok(since) = args[5].parse::<u64>() else {
        return 1;
    };
    worker_deadline();
    let path = remuda_native::daemon::socket_path("default");
    let request = remuda_core::protocol::Request::Sync {
        name: args[2].clone(),
        instance_id: Some(args[3].clone()),
        since,
        timeout_ms: 750,
    };
    let response = if args[0] == "local" {
        remuda_native::client::request_with_timeout(&path, &request, Duration::from_secs(3))
            .map_err(|_| ())
    } else {
        let fingerprint = &args[1];
        let current = || discover_peers().is_ok_and(|peers| peer_is_unique(&peers, fingerprint));
        if !current() {
            return 1;
        }
        let (address, pinned_static_key) =
            match remuda_native::cluster_api::resolve_call(fingerprint, None) {
                Ok(remuda_native::cluster_api::ResolvedCall::Remote {
                    fingerprint: resolved,
                    address,
                    pinned_static_key,
                }) if resolved == *fingerprint => (address, pinned_static_key),
                _ => return 1,
            };
        let private = match remuda_native::cluster::identity::load_static_private_key() {
            Ok(key) => key,
            Err(_) => return 1,
        };
        let response = remuda_native::net::cluster_client::ClusterClient::system()
            .request(address, &pinned_static_key, &private, &request)
            .map_err(|_| ());
        if !current() {
            return 1;
        }
        response
    };
    let result = response
        .and_then(|r| remuda_native::session_preview::decode_sync(r, &args[3]).map_err(|_| ()))
        .and_then(|f| {
            let version = f.output_version.ok_or(())?;
            if version < since {
                return Err(());
            }
            let instance = f.instance_id.clone().ok_or(())?;
            structured(f)
                .map(|payload| SyncFrame {
                    text: payload.text,
                    cells: payload.cells,
                    version,
                    instance,
                })
                .map_err(|_| ())
        });
    match result {
        Ok(frame) => {
            if abi2 {
                crate::write_worker_reply_abi(&encode_sync(&frame), Some(crate::WORKER_ABI_2))
            } else {
                // ABI 1 knew text/version/instance only; never send it the
                // larger cell grid it did not ask for.
                let legacy = SyncFrame {
                    text: frame.text,
                    cells: Vec::new(),
                    version: frame.version,
                    instance: frame.instance,
                };
                crate::write_worker_reply_abi(&legacy, Some(crate::WORKER_ABI))
            }
        }
        Err(()) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::agent::{Cursor, StyledCell};

    fn tagged<T: Serialize>(abi: &str, payload: &T) -> Vec<u8> {
        serde_json::to_vec(&crate::WorkerReply {
            abi: abi.to_owned(),
            payload,
        })
        .unwrap()
    }

    fn payload(cells: Vec<Vec<StyledCell>>, text: &str) -> PreviewPayload {
        PreviewPayload {
            text: text.into(),
            cells,
        }
    }

    #[test]
    fn extended_sync_arguments_are_not_accepted_as_legacy_preview() {
        let args = ["local", "", "session", "instance", "--sync", "0", "abi"].map(str::to_string);
        assert_eq!(worker(&args, None), 1);
    }

    #[test]
    fn abi_v1_keeps_a_string_reply_and_v2_carries_runs() {
        let cells = vec![vec![
            StyledCell {
                text: "e\u{301}".into(),
                fg: Color::Idx(1),
                bold: true,
                ..Default::default()
            },
            StyledCell {
                text: "🏇".into(),
                wide: true,
                ..Default::default()
            },
            StyledCell {
                text: String::new(),
                ..Default::default()
            },
        ]];
        let payload = payload(cells, "e\u{301}🏇\n");
        let v1 = tagged(crate::WORKER_ABI, &payload.text);
        let envelope = serde_json::from_slice::<crate::WorkerReply<String>>(&v1).unwrap();
        assert_eq!(envelope.abi, crate::WORKER_ABI);
        assert_eq!(
            envelope.payload, "e\u{301}🏇\n",
            "v1's payload stays a JSON string"
        );
        let decoded = read_preview(&v1).unwrap();
        assert_eq!(decoded.text, "e\u{301}🏇\n");
        assert!(decoded.cells.is_empty(), "v1 has no structured cells");

        let v2 = tagged(crate::WORKER_ABI_2, &encode_preview(&payload));
        assert!(
            serde_json::from_slice::<crate::WorkerReply<WirePreview>>(&v2).is_ok(),
            "v2's payload is tagged and structured"
        );
        let decoded = read_preview(&v2).unwrap();
        assert_eq!(decoded.text, "e\u{301}🏇\n");
        assert_eq!(decoded.cells[0][0].text, "e\u{301}");
        assert_eq!(decoded.cells[0][0].fg, Color::Idx(1));
        assert!(decoded.cells[0][0].bold);
        assert_eq!(decoded.cells[0][1].text, "🏇");
        assert!(decoded.cells[0][1].wide);
    }

    #[test]
    fn legacy_untagged_string_and_wrong_abi_are_handled_explicitly() {
        let legacy = serde_json::to_vec(&"old text\n").unwrap();
        let decoded = read_preview(&legacy).unwrap();
        assert_eq!(decoded.text, "old text\n");
        assert!(decoded.cells.is_empty());
        let wrong = tagged("--remuda-gui-worker-abi-9", &"text");
        assert_eq!(read_preview(&wrong).unwrap_err(), crate::STALE_HELPER);
        assert_eq!(
            read_preview(br#"{"abi":"wrong","payload":[]}"#).unwrap_err(),
            crate::STALE_HELPER
        );
    }

    #[test]
    fn compact_wire_preserves_combining_and_zwj_cell_boundaries() {
        let family = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
        let cells = vec![vec![
            StyledCell {
                text: "e\u{301}".into(),
                ..Default::default()
            },
            StyledCell {
                text: family.into(),
                ..Default::default()
            },
            StyledCell {
                text: "한".into(),
                wide: true,
                ..Default::default()
            },
            StyledCell {
                text: String::new(),
                ..Default::default()
            },
        ]];
        let wire = WirePreview::from_payload(&payload(cells, "e\u{301}👨‍👩‍👧‍👦한\n"));
        let back = wire.into_payload();
        assert_eq!(back.cells[0].len(), 3, "four scalars, three cells");
        assert_eq!(back.cells[0][0].text, "e\u{301}");
        assert_eq!(back.cells[0][1].text, family);
        assert_eq!(back.cells[0][2].text, "한");
        assert!(back.cells[0][2].wide);
    }

    fn grid(cols: usize, rows: usize, style_band: usize) -> PreviewPayload {
        let mut text = String::new();
        let cells = (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| {
                        let wide = (row + col).is_multiple_of(17);
                        let glyph = if wide { "한" } else { "x" };
                        text.push_str(glyph);
                        let band = (col / style_band.max(1)) as u8;
                        StyledCell {
                            text: glyph.into(),
                            fg: Color::Idx(band % 8),
                            bg: Color::Idx((band + 1) % 8),
                            bold: band.is_multiple_of(3),
                            underline: band.is_multiple_of(5),
                            wide,
                            ..Default::default()
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        payload(cells, &text)
    }

    #[test]
    fn structured_reply_fits_the_worker_stdout_limit_at_large_grids() {
        for (cols, rows) in [(120, 60), (200, 60)] {
            // One run per cell is the worst practical case for the run shape.
            let payload = grid(cols, rows, 1);
            let verbose = tagged(crate::WORKER_ABI_2, &payload);
            let wire = encode_preview(&payload);
            assert!(!wire.rows.is_empty(), "{cols}x{rows} keeps structured cells");
            let compact = tagged(crate::WORKER_ABI_2, &wire);
            assert!(
                compact.len() <= crate::WIRE_BUDGET,
                "{cols}x{rows} compact reply is {} bytes",
                compact.len()
            );
            assert!(
                compact.len() <= crate::provider::STDOUT_LIMIT,
                "{cols}x{rows} stays under the provider cap"
            );
            assert!(
                compact.len() < verbose.len(),
                "{cols}x{rows}: compact {} < verbose {}",
                compact.len(),
                verbose.len()
            );
            if cols == 200 {
                assert!(
                    verbose.len() > crate::provider::STDOUT_LIMIT,
                    "the old per-cell shape was {} bytes, over the cap",
                    verbose.len()
                );
            }
            let back = read_preview(&compact).unwrap();
            assert_eq!(back.cells.len(), rows);
            assert_eq!(back.cells[0].len(), cols);
        }
    }

    #[test]
    fn an_oversized_structured_reply_falls_back_to_text_instead_of_the_cap() {
        let cells = (0..65_536u32)
            .map(|i| StyledCell {
                text: "x".into(),
                // A different colour each cell means one run per cell.
                fg: Color::Idx((i % 256) as u8),
                bold: i.is_multiple_of(2),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let payload = payload(vec![cells], &"x".repeat(65_536));
        let wire = encode_preview(&payload);
        assert!(
            wire.rows.is_empty(),
            "the oversized grid is dropped, not silently truncated mid-row"
        );
        let reply = tagged(crate::WORKER_ABI_2, &wire);
        assert!(reply.len() <= crate::WIRE_BUDGET, "{}", reply.len());
        assert!(reply.len() <= crate::provider::STDOUT_LIMIT);
        let back = read_preview(&reply).unwrap();
        assert!(back.cells.is_empty());
        assert_eq!(back.text.len(), 65_536);
    }

    #[test]
    fn sync_v1_text_only_and_v2_structured_replies_are_both_readable() {
        let frame = SyncFrame {
            text: "sync\n".into(),
            cells: vec![vec![StyledCell {
                text: "s".into(),
                fg: Color::Idx(2),
                ..Default::default()
            }]],
            version: 7,
            instance: "i".into(),
        };
        let v1 = tagged(
            crate::WORKER_ABI,
            &SyncFrame {
                text: frame.text.clone(),
                cells: Vec::new(),
                version: frame.version,
                instance: frame.instance.clone(),
            },
        );
        let decoded = read_sync(&v1).unwrap();
        assert_eq!(decoded.version, 7);
        assert_eq!(decoded.text, "sync\n");
        assert!(decoded.cells.is_empty(), "v1 stays text-only");

        let v2 = tagged(crate::WORKER_ABI_2, &encode_sync(&frame));
        let decoded = read_sync(&v2).unwrap();
        assert_eq!(decoded.version, 7);
        assert_eq!(decoded.cells[0][0].text, "s");
        assert_eq!(decoded.cells[0][0].fg, Color::Idx(2));
        assert_eq!(
            read_sync(&tagged(crate::WORKER_ABI_2, &"text")).unwrap_err(),
            crate::STALE_HELPER
        );
    }

    fn preview_frame(cells: Vec<Vec<StyledCell>>) -> PreviewFrame {
        PreviewFrame {
            cells,
            wrapped: vec![false],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
            scrollback_len: 0,
            scrollback_total: 0,
            instance_id: Some("1".into()),
            output_version: Some(1),
        }
    }

    /// The real path from a captured grid through structured filtering, ABI 2
    /// serialization and the GUI frame must keep ZWJ/tag clusters and keep a
    /// stripped-only cell as its own column.
    #[test]
    fn zwj_tags_and_stripped_cells_survive_the_structured_wire() {
        let family = "👨\u{200D}👩\u{200D}👧";
        let flag = "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        let frame = preview_frame(
            vec![vec![
                StyledCell {
                    text: family.into(),
                    ..Default::default()
                },
                StyledCell {
                    text: "🏇".into(),
                    wide: true,
                    ..Default::default()
                },
                StyledCell {
                    text: "\u{202e}\u{1b}\u{200b}".into(),
                    ..Default::default()
                },
                StyledCell {
                    text: "X".into(),
                    ..Default::default()
                },
                StyledCell {
                    text: flag.into(),
                    ..Default::default()
                },
                StyledCell {
                    text: "\u{E0041}".into(),
                    ..Default::default()
                },
            ]],
        );
        let payload = structured(frame).unwrap();
        let row = &payload.cells[0];
        assert_eq!(row[0].text, family, "legitimate ZWJ is preserved");
        assert_eq!(row[2].text, " ", "a stripped-only cell keeps a column");
        assert_eq!(row[3].text, "X");
        assert_eq!(row[4].text, flag, "a valid tag sequence is preserved");
        assert_eq!(row[5].text, " ", "a stray tag char is filtered to a blank");
        assert!(!payload.text.contains('\u{202e}'));
        assert!(!payload.text.contains('\u{1b}'));
        assert!(!payload.text.contains('\u{200b}'));
        assert!(payload.text.contains(family));

        let back = read_preview(&tagged(crate::WORKER_ABI_2, &encode_preview(&payload))).unwrap();
        let frame = crate::terminal::frame_from_styled(back.cells);
        let cells = &frame.cells[0];
        assert_eq!(cells[0].text, family);
        assert!(cells[1].wide);
        assert_eq!(cells[2].text, " ", "the blank column is not a continuation");
        assert!(!cells[2].continuation);
        assert_eq!(cells[3].text, "X");
        assert_eq!(cells[4].text, flag);
    }

    fn wire_run(text: &str, wide: bool) -> StyledRun {
        StyledRun {
            text: text.into(),
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            wide,
        }
    }

    /// The real strict path, not a fabricated PreviewFrame: Response::Sync →
    /// decode_sync (scalar cells, controls stripped) → structured → ABI 2 →
    /// visible grid. A bidi-only cell keeps its column and the emoji/tag
    /// scalars are rejoined into clusters.
    #[test]
    fn real_strict_sync_wire_keeps_bidi_column_and_emoji_clusters() {
        let family = "👨\u{200D}👩\u{200D}👧";
        let flag = "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        let snapshot = remuda_core::protocol::StyledScreen {
            rows: vec![vec![
                wire_run(family, true),
                wire_run("🏇", true),
                wire_run("\u{202e}", false),
                wire_run("X", false),
                wire_run(flag, true),
            ]],
            bracketed_paste: false,
            wrapped: vec![false],
            scrollback_len: 0,
            scrollback_total: 0,
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
        };
        let frame = remuda_native::session_preview::decode_sync(
            remuda_core::protocol::Response::Sync {
                instance_id: "i".into(),
                output_version: 3,
                snapshot,
            },
            "i",
        )
        .unwrap();
        assert!(
            frame.cells[0].iter().any(|cell| cell.text.is_empty()),
            "strict decode empties the bidi-only cell before structured"
        );
        let payload = structured(frame).unwrap();
        assert_eq!(payload.text, format!("{family}🏇 X{flag}\n"));
        let back = read_preview(&tagged(crate::WORKER_ABI_2, &encode_preview(&payload))).unwrap();
        let frame = crate::terminal::frame_from_styled(back.cells);
        let cells = &frame.cells[0];
        assert_eq!(cells.len(), 5, "{cells:?}");
        assert_eq!(cells[0].text, family);
        assert!(cells[0].wide);
        assert_eq!(cells[1].text, "🏇");
        assert!(cells[1].wide);
        assert_eq!(cells[2].text, " ");
        assert!(!cells[2].continuation, "the bidi column stays a real column");
        assert_eq!(cells[3].text, "X");
        assert_eq!(cells[4].text, flag);
        assert!(cells[4].wide);
    }

    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("worker");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn abi2_rejection_falls_back_to_read_only_abi1_once() {
        let dir = std::env::temp_dir().join(format!("remuda-abi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("count");
        let worker = script(
            &dir,
            &format!(
                "#!/bin/sh\necho x >> '{}'\ncase \" $* \" in *\" --remuda-gui-worker-abi-2 \"*) exit 1 ;; esac\ncase \" $* \" in *\" --sync \"*) printf '%s' '{{\"abi\":\"--remuda-gui-worker-abi-1\",\"payload\":{{\"text\":\"sync\",\"version\":9,\"instance\":\"i\"}}}}' ;; *) printf '%s' '{{\"abi\":\"--remuda-gui-worker-abi-1\",\"payload\":\"legacy text\"}}' ;; esac\n",
                counter.display()
            ),
        );
        let selection = Selection {
            peer: None,
            name: "s".into(),
            instance: "i".into(),
        };
        let payload = request_preview(&worker, &selection).unwrap();
        assert_eq!(payload.text, "legacy text");
        assert!(payload.cells.is_empty());
        let frame = request_sync(&worker, &selection, 0).unwrap();
        assert_eq!(frame.version, 9);
        assert!(frame.cells.is_empty());
        let calls = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(calls, 4, "each read tries ABI 2 and then ABI 1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn silent_abi2_failure_recovers_on_next_read() {
        let dir = std::env::temp_dir().join(format!("remuda-abi-silent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let flag = dir.join("failed-once");
        let worker = script(&dir, &format!(
            "#!/bin/sh\ncase \" $* \" in *\" --remuda-gui-worker-abi-2 \"*) if [ ! -f '{flag}' ]; then : > '{flag}'; exit 1; fi; printf '%s' '{{\"abi\":\"--remuda-gui-worker-abi-2\",\"payload\":{{\"text\":\"structured\",\"rows\":[[{{\"t\":\"ok\"}}]]}}}}'; exit 0 ;; esac\nprintf '%s' '{{\"abi\":\"--remuda-gui-worker-abi-1\",\"payload\":\"legacy\"}}'\n",
            flag = flag.display(),
        ));
        let selection = Selection { peer: None, name: "s".into(), instance: "i".into() };
        let first = request_preview(&worker, &selection).unwrap();
        assert_eq!(first.text, "legacy");
        assert!(first.cells.is_empty());
        let second = request_preview(&worker, &selection).unwrap();
        assert_eq!(second.text, "structured");
        assert!(!second.cells.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_worker_that_rejects_both_tags_reports_an_error() {
        let dir = std::env::temp_dir().join(format!("remuda-abi-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let worker = script(&dir, "#!/bin/sh\nexit 1\n");
        let selection = Selection {
            peer: None,
            name: "s".into(),
            instance: "i".into(),
        };
        assert!(request_preview(&worker, &selection).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore]
    fn real_old_worker_negotiates_abi1_through_provider() {
        let path = std::env::var_os("REMUDA_GUI_ACCEPTANCE_OLD_WORKER")
            .expect("REMUDA_GUI_ACCEPTANCE_OLD_WORKER");
        let name = std::env::var("REMUDA_GUI_ACCEPTANCE_NAME").expect("name");
        let instance = std::env::var("REMUDA_GUI_ACCEPTANCE_INSTANCE").expect("instance");
        let selection = Selection {
            peer: None,
            name,
            instance,
        };
        let payload = request_preview(Path::new(&path), &selection).expect("preview");
        assert!(!payload.text.is_empty());
        assert!(payload.cells.is_empty(), "old worker is text-only");
        let frame = request_sync(Path::new(&path), &selection, 0).expect("sync");
        assert_eq!(frame.instance, selection.instance);
        assert!(frame.cells.is_empty());
    }

    #[test]
    fn structured_keeps_style_and_wide_cells_while_filtering_text() {
        let frame = PreviewFrame {
            cells: vec![vec![
                StyledCell {
                    text: "a\u{1b}\u{202e}b".into(),
                    fg: Color::Idx(1),
                    bold: true,
                    ..Default::default()
                },
                StyledCell {
                    text: "🏇".into(),
                    wide: true,
                    ..Default::default()
                },
            ]],
            wrapped: vec![false],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
            scrollback_len: 0,
            scrollback_total: 0,
            instance_id: Some("1".into()),
            output_version: None,
        };
        let payload = structured(frame).unwrap();
        assert_eq!(payload.cells[0][0].text, "ab");
        assert_eq!(payload.cells[0][0].fg, Color::Idx(1));
        assert!(payload.cells[0][0].bold);
        assert!(payload.cells[0][1].wide);
        assert_eq!(payload.cells[0][1].text, "🏇");
        assert_eq!(payload.text, "ab🏇\n");
    }

    #[test]
    fn text_removes_controls_and_directional_formatting() {
        let frame = PreviewFrame {
            cells: vec![vec![StyledCell {
                text: "a\u{1b}\u{202e}b".into(),
                ..Default::default()
            }]],
            wrapped: vec![],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
            scrollback_len: 0,
            scrollback_total: 0,
            instance_id: Some("1".into()),
            output_version: None,
        };
        assert_eq!(plain(frame).unwrap(), "ab\n");
    }
}
