//! The OSC 7501 title must survive both session backends and reach the local
//! Lua listing unchanged. The child is synthetic on purpose: this checks the
//! assembled daemon path, not an agent's prompt or a model call.
#![cfg(unix)]
#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Mode, Parity};
use std::time::{Duration, Instant};

const SCRIPT: &str = "printf '\\033]7501;state=working:app=opencode:title=UmV2aWV3IE9TQyB0YXNr:msg=c2VjcmV0\\033\\\\'; sleep 30";

fn listed(daemon: &Parity) -> (String, String) {
    daemon
        .eval(
            r#"
            for _, row in ipairs(remuda.session.list()) do
              if row.name == "task" then
                local status = row.program_status
                if type(status) ~= "table" then return "\n" end
                return tostring(status.state) .. "\n" .. tostring(status.title or "")
              end
            end
            return "\n"
            "#,
        )
        .expect("list sessions")
        .split_once('\n')
        .map(|(state, title)| (state.to_owned(), title.to_owned()))
        .unwrap_or_default()
}

#[test]
fn task_title_reaches_lua_for_in_process_and_hosted_sessions() {
    for mode in [Mode::InProcess, Mode::Hosted] {
        let daemon = Parity::start(mode);
        daemon.start_command("task", vec!["/bin/sh".into(), "-c".into(), SCRIPT.into()]);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (state, title) = listed(&daemon);
            if state == "working" && title == "Review OSC task" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{mode:?}: OSC title never reached Lua: state={state:?} title={title:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
