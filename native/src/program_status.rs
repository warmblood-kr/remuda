//! Bounded Program Status Protocol (OSC 7501) tracking.
//!
//! The protocol is advisory: an agent says whether it is idle, working,
//! blocked, done or failed. Remuda never treats it as a process identity,
//! authorization, or proof of liveness. Free text fields (`title`, `msg`,
//! `id`) are intentionally not retained: base64 is not redaction.

use base64::Engine;
use remuda_core::agent::{ProgramBlockKind, ProgramState, ProgramStatus, ProgramStatusObservation};
use std::time::Instant;

/// The longest OSC 7501 payload Remuda will inspect. `Strings` already bounds
/// OSC accumulation; this keeps a direct vt100 path equally bounded.
const MAX_PAYLOAD: usize = 4096;
const MAX_APP: usize = 32;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE_DECODED: usize = 192;
const MAX_MESSAGE_ENCODED: usize = 2732;
const MAX_MESSAGE_DECODED: usize = 2048;

/// The fixed capability reply. It advertises only state/kind values Remuda
/// stores and never reflects bytes from the querying program.
pub(crate) const QUERY_REPLY: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

/// One terminal's last status and the query latch.
#[derive(Default)]
pub(crate) struct Tracker {
    status: Option<ProgramStatus>,
    updated_at: Option<Instant>,
    query_pending: bool,
}

impl Tracker {
    /// Observe one parsed OSC `params` slice. Unknown, malformed or oversized
    /// records leave the previous status intact; only `state=clear` removes it.
    pub fn observe(&mut self, params: &[&[u8]]) {
        let [op, payload] = params else { return };
        if *op != b"7501" || payload.len() > MAX_PAYLOAD {
            return;
        }
        let Ok(payload) = std::str::from_utf8(payload) else {
            return;
        };
        // Exact `?` is a feature query. Anything else starting with `?` is a
        // capability response; never answer it, or two terminals can loop.
        if payload == "?" {
            self.query_pending = true;
            return;
        }
        if payload.starts_with('?') {
            return;
        }
        let Some(record) = parse(payload) else { return };
        match record {
            Record::Clear => {
                self.status = None;
                self.updated_at = Some(Instant::now());
            }
            Record::Status(status) => {
                self.status = Some(status);
                self.updated_at = Some(Instant::now());
            }
        }
    }

    /// Take one pending feature query without repeating it.
    pub fn take_query(&mut self) -> bool {
        std::mem::take(&mut self.query_pending)
    }

    pub fn observation(&self) -> Option<ProgramStatusObservation> {
        let status = self.status.clone()?;
        let age = self.updated_at.map(|at| at.elapsed()).unwrap_or_default();
        Some(ProgramStatusObservation { status, age })
    }
}

enum Record {
    Clear,
    Status(ProgramStatus),
}

fn parse(payload: &str) -> Option<Record> {
    let mut state = None;
    let mut app = None;
    let mut kind = None;
    let mut progress = None;
    let mut title = None;
    let mut message = None;
    let mut has_id = false;

    for field in payload.split(':') {
        let Some((key, value)) = field.split_once('=') else {
            // A pair without `=` is an extension Remuda does not understand;
            // ignore it without invalidating the rest of the record.
            continue;
        };
        match key {
            // Earlier duplicate keys are replaced by later ones (last wins).
            "state" => state = parse_state(value),
            "app" => {
                app = valid_app(value).then(|| value.to_string());
            }
            "kind" => kind = parse_kind(value),
            "progress" => {
                progress = value.parse::<u8>().ok().filter(|n| *n <= 100);
            }
            // Root-only slice: an id-bearing report belongs to a child record,
            // so never apply it to the root (and never fall back to root).
            "id" => has_id = true,
            // Text is validated but never retained. Normalize duplicates first,
            // then reject controls/oversize in the winning value.
            "title" => title = Some(value),
            "msg" => message = Some(value),
            _ => {}
        }
    }

    if has_id {
        return None;
    }
    if let Some(value) = title {
        valid_text(value, MAX_TITLE_ENCODED, MAX_TITLE_DECODED)?;
    }
    if let Some(value) = message {
        valid_text(value, MAX_MESSAGE_ENCODED, MAX_MESSAGE_DECODED)?;
    }
    let state = state?;
    if state == ParsedState::Clear {
        return Some(Record::Clear);
    }
    let state = match state {
        ParsedState::Idle => ProgramState::Idle,
        ParsedState::Working => ProgramState::Working,
        ParsedState::Blocked => ProgramState::Blocked,
        ParsedState::Done => ProgramState::Done,
        ParsedState::Error => ProgramState::Error,
        ParsedState::Clear => unreachable!(),
    };
    // `kind` is defined only for blocked, and `progress` only for working or
    // blocked. Drop a field outside its state instead of rejecting a usable
    // state report.
    let kind = if state == ProgramState::Blocked {
        kind
    } else {
        None
    };
    let progress = if matches!(state, ProgramState::Working | ProgramState::Blocked) {
        progress
    } else {
        None
    };
    Some(Record::Status(ProgramStatus {
        state,
        app,
        kind,
        progress,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParsedState {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
    Clear,
}

fn parse_state(value: &str) -> Option<ParsedState> {
    match value {
        "idle" => Some(ParsedState::Idle),
        "working" => Some(ParsedState::Working),
        "blocked" => Some(ParsedState::Blocked),
        "done" => Some(ParsedState::Done),
        "error" => Some(ParsedState::Error),
        "clear" => Some(ParsedState::Clear),
        _ => None,
    }
}

fn parse_kind(value: &str) -> Option<ProgramBlockKind> {
    match value {
        "permission" => Some(ProgramBlockKind::Permission),
        "question" => Some(ProgramBlockKind::Question),
        "auth" => Some(ProgramBlockKind::Auth),
        _ => None,
    }
}

fn valid_text(value: &str, max_encoded: usize, max_decoded: usize) -> Option<()> {
    if value.len() > max_encoded {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value))
        .ok()?;
    if decoded.len() > max_decoded {
        return None;
    }
    let text = std::str::from_utf8(&decoded).ok()?;
    if text.chars().any(|c| {
        let n = c as u32;
        n < 0x20 || n == 0x7f || (0x80..=0x9f).contains(&n) || n == 0x1b
    }) {
        return None;
    }
    Some(())
}

fn valid_app(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_APP
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe(status: &mut Tracker, payload: &str) {
        status.observe(&[b"7501", payload.as_bytes()]);
    }

    #[test]
    fn states_kinds_and_progress_are_bounded_and_typed() {
        let mut status = Tracker::default();
        observe(&mut status, "state=working:app=codex:progress=42");
        let observed = status.observation().expect("working status");
        assert_eq!(observed.status.state, ProgramState::Working);
        assert_eq!(observed.status.app.as_deref(), Some("codex"));
        assert_eq!(observed.status.progress, Some(42));

        observe(&mut status, "state=blocked:app=claude-code:kind=permission");
        let observed = status.observation().expect("blocked status");
        assert_eq!(observed.status.kind, Some(ProgramBlockKind::Permission));
        assert_eq!(observed.status.progress, None, "progress is not implied");

        observe(&mut status, "state=done");
        let observed = status.observation().unwrap();
        assert_eq!(observed.status.state, ProgramState::Done);
        assert_eq!(observed.status.kind, None, "kind is blocked-only");
        assert_eq!(observed.status.progress, None, "progress is run-only");
    }

    #[test]
    fn malformed_records_keep_the_last_status_and_clear_is_explicit() {
        let mut status = Tracker::default();
        observe(&mut status, "state=idle:app=opencode");
        for bad in [
            "state=working:state=idle",
            "state=nonsense",
            "msg=not-a-state",
            "state=",
        ] {
            observe(&mut status, bad);
            assert_eq!(
                status.observation().unwrap().status.state,
                ProgramState::Idle,
                "{bad} must not replace a valid status"
            );
        }
        observe(&mut status, "state=working:app=bad app");
        assert_eq!(
            status.observation().unwrap().status.state,
            ProgramState::Working,
            "an invalid app is omitted, not the whole state"
        );
        observe(&mut status, "state=working:progress=101");
        let invalid_progress = status.observation().unwrap();
        assert_eq!(invalid_progress.status.state, ProgramState::Working);
        assert_eq!(
            invalid_progress.status.progress, None,
            "invalid progress is omitted"
        );
        observe(&mut status, "state=working:progress=-1:progress=10");
        assert_eq!(
            status.observation().unwrap().status.progress,
            Some(10),
            "later duplicate keys win"
        );
        observe(&mut status, "state=clear");
        assert!(status.observation().is_none(), "explicit clear removes it");
    }

    #[test]
    fn query_and_capability_response_do_not_toggle_each_other() {
        let mut status = Tracker::default();
        observe(&mut status, "?");
        assert!(status.take_query());
        assert!(!status.take_query(), "one query, one reply");
        observe(&mut status, "?:states=idle,working");
        assert!(!status.take_query(), "a capability response is not a query");
        assert!(status.observation().is_none());
    }

    #[test]
    fn unknown_fields_and_terminators_are_ignored_or_rejected() {
        let mut status = Tracker::default();
        observe(&mut status, "state=working:title=c2VjcmV0:msg=c2VjcmV0");
        let observed = status.observation().unwrap();
        assert_eq!(observed.status.state, ProgramState::Working);
        assert_eq!(observed.status.app, None);
        assert_eq!(observed.status.kind, None);
        assert_eq!(observed.status.progress, None);
        observe(&mut status, "state=working:future=extension");
        assert_eq!(
            status.observation().unwrap().status.state,
            ProgramState::Working
        );
    }

    #[test]
    fn multilingual_text_is_valid_but_controls_are_rejected() {
        use base64::Engine;
        let b64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        let mut status = Tracker::default();
        observe(
            &mut status,
            &format!(
                "state=working:msg={}:title={}",
                b64("한글 작업"),
                b64("🚀 배포")
            ),
        );
        assert_eq!(
            status.observation().unwrap().status.state,
            ProgramState::Working
        );
        observe(
            &mut status,
            &format!("state=blocked:msg={}", b64("line\u{85}break")),
        );
        assert_eq!(
            status.observation().unwrap().status.state,
            ProgramState::Working,
            "U+0085 is a control, not ordinary multilingual text"
        );
        observe(
            &mut status,
            &format!("state=blocked:msg={}", b64("safe\u{1b}[31m")),
        );
        assert_eq!(
            status.observation().unwrap().status.state,
            ProgramState::Working,
            "ESC in decoded text rejects the report"
        );
        observe(&mut status, "state=blocked:msg=//8=");
        assert_eq!(
            status.observation().unwrap().status.state,
            ProgramState::Working,
            "invalid UTF-8 rejects the report"
        );
    }

    #[test]
    fn child_records_and_bad_text_do_not_replace_the_root() {
        let mut status = Tracker::default();
        observe(&mut status, "state=idle");
        observe(&mut status, "state=working:id=child/1");
        assert_eq!(
            status.observation().unwrap().status.state,
            ProgramState::Idle,
            "an id-bearing child report must not become root"
        );
        observe(&mut status, "state=clear:id=child/1");
        assert!(
            status.observation().is_some(),
            "a child clear is not root clear"
        );
        observe(&mut status, "state=working:msg=not base64!");
        observe(&mut status, "state=working:title=SGVsbG8=\x1b");
        assert_eq!(
            status.observation().unwrap().status.state,
            ProgramState::Idle,
            "invalid free text rejects the whole report"
        );
    }
}
