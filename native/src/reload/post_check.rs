//! After `remuda reload` reports `reloaded:`, the CLI checks the new daemon
//! (#713): it runs the target build, every host the reload moved is listed
//! and alive, and its `init.lua` loaded. The old daemon is gone by then, so
//! a failed check cannot roll back; it says what is wrong.
use remuda_core::protocol::{Request, Response};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long `init.lua` may take to load in the new daemon.
pub const CONFIG_DEADLINE: Duration = Duration::from_secs(10);

/// Lines for the CLI, and whether every check passed.
#[derive(Debug, PartialEq, Eq)]
pub struct PostCheck {
    pub lines: Vec<String>,
    pub ok: bool,
}

/// The `init.lua` error of the daemon at `socket` now, if it has one: asked
/// before a reload, so the check fails only on a failure the reload brought.
pub fn config_error_before(socket: &Path) -> Option<String> {
    match config_status(socket, Duration::ZERO) {
        Config::Failed(error) => Some(error),
        _ => None,
    }
}

/// Check the daemon at `socket` against a reload's `report`. `before` is
/// [`config_error_before`] from the old daemon.
pub fn post_check(
    socket: &Path,
    report: &str,
    before: Option<&str>,
    config_deadline: Duration,
) -> PostCheck {
    let mut lines = Vec::new();
    let mut ok = true;
    let target = report
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("reloaded: "))
        .and_then(|line| line.strip_suffix(" now serves this socket"));
    match crate::client::request(socket, &Request::Version) {
        Ok(Response::Value(build)) if Some(build.as_str()) == target || target.is_none() => {
            lines.push(format!("daemon now {build}"));
        }
        Ok(Response::Value(build)) => {
            ok = false;
            lines.push(format!(
                "daemon now {build}, not {}: another daemon answers",
                target.unwrap_or("?")
            ));
        }
        other => {
            return PostCheck {
                lines: vec![format!(
                    "after the reload the daemon did not answer: {other:?}"
                )],
                ok: false,
            }
        }
    }
    let moved: Vec<&str> = report
        .lines()
        .filter_map(|line| line.strip_prefix("host "))
        .filter_map(|line| line.split_once(':').map(|(name, _)| name))
        .collect();
    match crate::client::request(socket, &Request::List) {
        Ok(Response::Sessions(sessions)) => {
            let missing: Vec<&str> = moved
                .iter()
                .copied()
                .filter(|name| !sessions.iter().any(|s| s.name == *name && s.alive))
                .collect();
            if missing.is_empty() {
                lines.push(format!("sessions: all {} host-backed listed", moved.len()));
            } else {
                ok = false;
                lines.push(format!("sessions missing or dead: {}", missing.join(", ")));
            }
        }
        other => {
            ok = false;
            lines.push(format!("sessions: the daemon answered {other:?}"));
        }
    }
    match config_status(socket, config_deadline) {
        Config::Loaded => lines.push("init.lua: loaded".into()),
        Config::Failed(error) if before == Some(error.as_str()) => {
            lines.push(format!(
                "init.lua failed, as it did before the reload: {error}"
            ));
        }
        Config::Failed(error) => {
            ok = false;
            lines.push(format!("init.lua failed in the new daemon: {error}"));
        }
        Config::Loading => {
            lines.push(format!(
                "init.lua: still loading after {} s",
                config_deadline.as_secs()
            ));
        }
        Config::Unknown(why) => lines.push(format!("init.lua: status unknown ({why})")),
    }
    PostCheck { lines, ok }
}

enum Config {
    Loaded,
    Failed(String),
    Loading,
    Unknown(String),
}

/// Poll the daemon's own record of its `init.lua` load.
fn config_status(socket: &Path, deadline: Duration) -> Config {
    // `keymap_snapshot` carries the daemon's own record of the load.
    const ASK: &str = "local s = remuda.keymap_snapshot() \
                       return (s.ready and 'done' or 'loading') .. '\\t' .. (s.diagnostic or '')";
    let until = Instant::now() + deadline;
    loop {
        let answer = crate::client::request(
            socket,
            &Request::Eval {
                code: ASK.into(),
                name: Some("@remuda/reload-post-check".into()),
            },
        );
        match answer {
            Ok(Response::Value(text)) => match text.split_once('\t') {
                Some(("done", "")) => return Config::Loaded,
                Some(("done", error)) => return Config::Failed(error.into()),
                Some(("loading", _)) if Instant::now() < until => {}
                Some(("loading", _)) => return Config::Loading,
                _ => return Config::Unknown(text),
            },
            other => return Config::Unknown(format!("{other:?}")),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
