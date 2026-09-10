//! Generating a session name, on a machine with no pty.
//!
//! This is the one piece of new branching logic the usability rework adds to
//! the policy layer, and it is here rather than in the CLI precisely so it can
//! be exercised against `ScriptedAgent` — the property `steps/012` exists to
//! protect is that the human-facing work did not grow code only a human can
//! check.

use remuda_core::registry::slug;
use remuda_core::{Registry, ScriptedAgent, Session};
use std::sync::Arc;

fn session(name: &str) -> Session {
    Session::new(
        name,
        Box::new(ScriptedAgent::new(vec![])),
        Arc::new(remuda_core::ManualClock::new()),
    )
}

#[test]
fn a_name_comes_from_the_program_not_from_the_person() {
    assert_eq!(slug("claude"), "claude");
    assert_eq!(slug("/usr/bin/zsh"), "zsh");
    assert_eq!(slug(r"C:\Windows\System32\cmd.exe"), "cmd-exe");
    assert_eq!(slug("Claude-Code"), "claude-code");
    assert_eq!(slug("my prog!"), "my-prog");
}

#[test]
fn a_name_that_would_be_empty_falls_back_rather_than_being_empty() {
    // An empty key is addressable by nobody, so it must never be produced.
    assert_eq!(slug("///"), "session");
    assert_eq!(slug(""), "session");
    assert_eq!(slug("한글"), "session");
}

#[test]
fn the_second_claude_is_claude_2() {
    let registry = Registry::new();
    assert_eq!(registry.unique_name("claude"), "claude");

    registry.register(session("claude")).expect("first");
    assert_eq!(registry.unique_name("claude"), "claude-2");

    registry.register(session("claude-2")).expect("second");
    assert_eq!(registry.unique_name("claude"), "claude-3");
}

#[test]
fn a_gap_is_filled_rather_than_skipped() {
    // The counter is not stored anywhere, so closing claude-2 makes the name
    // free again. That is a property of asking the registry, not a coincidence.
    let registry = Registry::new();
    registry.register(session("claude")).expect("first");
    registry.register(session("claude-3")).expect("third");
    assert_eq!(registry.unique_name("claude"), "claude-2");
}

#[test]
fn a_summary_says_whether_a_human_holds_it() {
    let registry = Registry::new();
    let handle = registry.register(session("ridden")).expect("registered");
    assert!(!registry.list()[0].attached, "nobody is attached yet");

    let held = handle.attach().expect("attach");
    assert!(
        registry.list()[0].attached,
        "the TUI draws its flag from this"
    );
    drop(held);
    assert!(!registry.list()[0].attached);
}
