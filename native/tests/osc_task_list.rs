//! The OSC 7501 title must survive both session backends and reach the local
//! Lua listing unchanged. The child is synthetic on purpose: this checks the
//! assembled daemon path, not an agent's prompt or a model call.
#![cfg(unix)]
#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Mode, Parity};
use std::time::{Duration, Instant};

const SCRIPT: &str = "printf '\\033]7501;state=working:app=opencode:title=UmV2aWV3IE9TQyB0YXNr:msg=c2VjcmV0\\033\\\\'; sleep 30";

fn listed(daemon: &Parity) -> (String, String, f64, f64) {
    let out = daemon
        .eval(
            r#"
            for _, row in ipairs(remuda.session.list()) do
              if row.name == "task" then
                local status = row.program_status
                if type(status) ~= "table" then return "\n" end
                return tostring(status.state) .. "\n" .. tostring(status.title or "")
                  .. "\n" .. tostring(status.last_updated_at or "")
                  .. "\n" .. tostring(row.program_status_age or "")
              end
            end
            return "\n"
            "#,
        )
        .expect("list sessions");
    let mut parts = out.split('\n');
    let state = parts.next().unwrap_or_default().to_owned();
    let title = parts.next().unwrap_or_default().to_owned();
    let updated = parts.next().unwrap_or_default().parse().unwrap_or(0.0);
    let age = parts.next().unwrap_or_default().parse().unwrap_or(-1.0);
    (state, title, updated, age)
}

fn reported_at(daemon: &Parity) -> Option<f64> {
    let value = daemon
        .eval(
            r#"
            for _, row in ipairs(remuda.session.list()) do
              if row.name == "task" and type(row.program_status) == "table"
                and row.program_status.last_updated_at then
                return tostring(row.program_status.last_updated_at)
              end
            end
            return ""
            "#,
        )
        .ok()?;
    value.parse().ok()
}

fn wait_for_reported_at(daemon: &Parity) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(at) = reported_at(daemon) {
            return at;
        }
        assert!(
            Instant::now() < deadline,
            "the report time never reached Lua after the restart"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_hosted_report_time_survives_a_daemon_kill_and_restart() {
    let mut daemon = Parity::start(Mode::Hosted);
    daemon.start_command("task", vec!["/bin/sh".into(), "-c".into(), SCRIPT.into()]);
    let before = wait_for_reported_at(&daemon);

    // A daemon crash must not reset a hosted session's report time: the host
    // owns the tracker, and the restarted daemon reattaches to it.
    daemon.kill_daemon();
    daemon.restart();

    let after = wait_for_reported_at(&daemon);
    assert_eq!(
        before, after,
        "a daemon restart must not move the report time"
    );
}

#[test]
fn task_title_reaches_lua_for_in_process_and_hosted_sessions() {
    for mode in [Mode::InProcess, Mode::Hosted] {
        let daemon = Parity::start(mode);
        daemon.start_command("task", vec!["/bin/sh".into(), "-c".into(), SCRIPT.into()]);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (state, title, _, _) = listed(&daemon);
            if state == "working" && title == "Review OSC task" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{mode:?}: OSC title never reached Lua: state={state:?} title={title:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let (_, _, first_at, first_age) = listed(&daemon);
        assert!(
            (1_700_000_000.0..2_000_000_000.0).contains(&first_at),
            "{mode:?}: last_updated_at is not a UTC Unix timestamp: {first_at}"
        );
        std::thread::sleep(Duration::from_millis(1200));
        let (_, _, second_at, second_age) = listed(&daemon);
        assert_eq!(
            first_at, second_at,
            "{mode:?}: a read without a new report moved the timestamp"
        );
        assert!(
            second_age > first_age,
            "{mode:?}: the rendered age must grow: {first_age} -> {second_age}"
        );
    }
}
