//! Bounded Program Status Protocol (OSC 7501) tracking.
//!
//! The protocol is advisory: an agent says whether it is idle, working,
//! blocked, done or failed. Remuda never treats it as a process identity,
//! authorization, or proof of liveness.
//!
//! The optional `title` is retained as bounded, control-free, local display
//! text. `msg` and child `id` records are still discarded: a message is often
//! the question or the user's prompt, and base64 is not redaction.

use base64::Engine;
use remuda_core::agent::{ProgramBlockKind, ProgramState, ProgramStatus, ProgramStatusObservation};
use std::time::Instant;

/// The longest OSC 7501 payload Remuda will inspect. `Strings` already bounds
/// OSC accumulation; this keeps a direct vt100 path equally bounded.
const MAX_PAYLOAD: usize = 4096;
const MAX_APP: usize = 32;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE_DECODED: usize = 192;

/// One terminal's last typed status. Queries are observed and ignored.
#[derive(Default)]
pub(crate) struct Tracker {
    status: Option<ProgramStatus>,
    title: Option<String>,
    updated_at: Option<Instant>,
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
        // Query handling belongs to the terminal callback, not the tracker.
        if payload.starts_with('?') {
            return;
        }
        let Some(record) = parse(payload) else { return };
        match record {
            Record::Clear => {
                self.status = None;
                self.title = None;
                self.updated_at = Some(Instant::now());
            }
            Record::Status(status, title) => {
                self.status = Some(status);
                self.title = title;
                self.updated_at = Some(Instant::now());
            }
        }
    }

    pub fn observation(&self) -> Option<ProgramStatusObservation> {
        let status = self.status.clone()?;
        let age = self.updated_at.map(|at| at.elapsed()).unwrap_or_default();
        Some(ProgramStatusObservation {
            status,
            age,
            title: self.title.clone(),
        })
    }
}

enum Record {
    Clear,
    Status(ProgramStatus, Option<String>),
}

fn parse(payload: &str) -> Option<Record> {
    let mut state = None;
    let mut app = None;
    let mut kind = None;
    let mut progress = None;
    let mut title = None;
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
            // `title` is optional display text; `msg` and other free text stay
            // discarded. An invalid title drops only the title, never a usable
            // state: text is decoration, while the state is the reported fact.
            "title" => title = Some(value),
            _ => {}
        }
    }

    if has_id {
        return None;
    }
    let title = title
        .and_then(|value| decode_text(value, MAX_TITLE_ENCODED, MAX_TITLE_DECODED))
        .filter(|value| !value.is_empty());
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
    Some(Record::Status(
        ProgramStatus {
            state,
            app,
            kind,
            progress,
        },
        title,
    ))
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

fn decode_text(value: &str, max_encoded: usize, max_decoded: usize) -> Option<String> {
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
    let text = String::from_utf8(decoded).ok()?;
    if text.chars().any(|c| {
        let n = c as u32;
        n < 0x20 || n == 0x7f || (0x80..=0x9f).contains(&n) || unsafe_format(c)
    }) {
        return None;
    }
    Some(text)
}

/// Unicode format controls can reorder or hide text in a terminal. They are
/// not ordinary multilingual content; a title carrying one is not displayed.
fn unsafe_format(c: char) -> bool {
    let n = c as u32;
    n == 0x061c
        || (0x200b..=0x200f).contains(&n)
        || (0x202a..=0x202e).contains(&n)
        || (0x2028..=0x2029).contains(&n)
        || (0x2060..=0x2064).contains(&n)
        || (0x2066..=0x2069).contains(&n)
        || n == 0xfeff
        || (0xfff9..=0xfffb).contains(&n)
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
    fn unknown_fields_and_terminators_are_ignored_or_rejected() {
        let mut status = Tracker::default();
        observe(&mut status, "state=working:title=c2VjcmV0:msg=c2VjcmV0");
        let observed = status.observation().unwrap();
        assert_eq!(observed.status.state, ProgramState::Working);
        assert_eq!(observed.status.app, None);
        assert_eq!(observed.status.kind, None);
        assert_eq!(observed.status.progress, None);
        assert_eq!(
            observed.title.as_deref(),
            Some("secret"),
            "title is retained"
        );
        observe(&mut status, "state=working:future=extension");
        let observed = status.observation().unwrap();
        assert_eq!(observed.status.state, ProgramState::Working);
        assert_eq!(
            observed.title, None,
            "a later report without a title does not keep the old one"
        );
    }

    #[test]
    fn title_is_bounded_clean_multilingual_text() {
        use base64::Engine;
        let b64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        let mut status = Tracker::default();
        observe(
            &mut status,
            &format!("state=working:title={}", b64("🚀 배포 — OSC 상태")),
        );
        let observed = status.observation().unwrap();
        assert_eq!(observed.status.state, ProgramState::Working);
        assert_eq!(observed.title.as_deref(), Some("🚀 배포 — OSC 상태"));

        observe(
            &mut status,
            &format!("state=working:title={}", b64(&"x".repeat(193))),
        );
        let observed = status.observation().unwrap();
        assert_eq!(
            observed.status.state,
            ProgramState::Working,
            "an oversized title does not erase the state"
        );
        assert_eq!(observed.title, None, "an oversized title is not retained");
    }

    #[test]
    fn title_controls_format_characters_and_bad_utf8_are_dropped() {
        use base64::Engine;
        let b64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        let mut status = Tracker::default();
        observe(&mut status, &format!("state=working:title={}", b64("safe")));
        for bad in [
            "line\u{85}break",
            "safe\u{1b}[31m",
            "spoof\u{202e}gnp.exe",
            "zero\u{200b}width",
            "para\u{2029}graph",
        ] {
            observe(&mut status, &format!("state=blocked:title={}", b64(bad)));
            let observed = status.observation().unwrap();
            assert_eq!(
                observed.status.state,
                ProgramState::Blocked,
                "{bad:?} must not erase a usable state"
            );
            assert_eq!(observed.title, None, "{bad:?} must not be displayed");
        }
        observe(&mut status, "state=blocked:title=//8=");
        let observed = status.observation().unwrap();
        assert_eq!(observed.status.state, ProgramState::Blocked);
        assert_eq!(observed.title, None, "invalid UTF-8 is not displayed");
    }

    #[test]
    fn child_records_and_clear_do_not_leak_a_title() {
        let mut status = Tracker::default();
        observe(&mut status, "state=idle");
        observe(&mut status, "state=working:id=child/1:title=c2VjcmV0");
        let observed = status.observation().unwrap();
        assert_eq!(
            observed.status.state,
            ProgramState::Idle,
            "an id-bearing child report must not become root"
        );
        assert_eq!(observed.title, None, "a child title is not root text");
        observe(&mut status, "state=clear:id=child/1");
        assert!(
            status.observation().is_some(),
            "a child clear is not root clear"
        );
        observe(&mut status, "state=clear");
        assert!(
            status.observation().is_none(),
            "root clear removes the title"
        );
    }
}
