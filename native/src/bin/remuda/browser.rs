use remuda_native::net::browser::{ssh_arguments, valid_ssh_destination as valid_destination};
use std::io::{self, IsTerminal};
use std::process::{Command, ExitCode};

const USAGE: &str =
    "Usage: remuda browser [[USER@]HOST]\n       remuda browser --gui [[USER@]HOST]\n       remuda browser open-document [--session] REQUEST_ID URI\n       remuda browser link URI [LABEL]";

pub(super) fn run(args: &[&str], server: &str, stdin_enabled: bool) -> ExitCode {
    if matches!(args, ["-h" | "--help" | "help"]) {
        println!(
            "{USAGE}\n\nWithout a host, open the local workspace or choose a remote host on macOS or Windows. With a host, connect through SSH. Use --gui for the native client.\nAn SSH config alias is accepted. The remote login account must have remuda on its PATH."
        );
        return ExitCode::SUCCESS;
    }
    if server != "default" || stdin_enabled {
        return super::fail(format!(
            "{USAGE}\nLocal --server and --stdin options are unsupported."
        ));
    }
    if let ["link", uri] | ["link", uri, _] = args {
        return match terminal_link(
            uri,
            args.get(2).copied().unwrap_or(uri),
            io::stdout().is_terminal(),
        ) {
            Ok(link) => {
                println!("{link}");
                ExitCode::SUCCESS
            }
            Err(reason) => super::fail(reason),
        };
    }
    if let ["open-document", request_id, uri] = args {
        return publish_document(request_id, uri, false);
    }
    if let ["open-document", "--session", request_id, uri] = args {
        return publish_document(request_id, uri, true);
    }
    if matches!(args, [] | ["--gui"]) {
        return launch_gui(&["--browser-auto"]);
    }
    if let ["--gui", destination] = args {
        if !valid_destination(destination) {
            return super::fail(USAGE);
        }
        return launch_gui(&["--browser", destination]);
    }
    let [destination] = args else {
        return super::fail(USAGE);
    };
    if !valid_destination(destination) {
        return super::fail(format!(
            "{USAGE}\nExpected an SSH hostname, IP address or config alias."
        ));
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return super::fail("remuda browser requires an interactive terminal");
    }

    // Keep the command on the server fixed. Host trust and user authentication
    // use the user's SSH configuration and known_hosts.
    match Command::new("ssh")
        .args(ssh_arguments(destination))
        .status()
    {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => super::fail(format!("remuda browser: could not run ssh: {error}")),
    }
}

#[cfg(any(target_os = "macos", windows))]
fn launch_gui(args: &[&str]) -> ExitCode {
    let cli = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => return super::fail(format!("cannot locate Remuda Browser: {error}")),
    };
    let executable = match super::gui_managed::choose(&cli, remuda_native::dist::BUILD_VERSION) {
        Ok(Some(path)) => path,
        Ok(None) => return super::fail("Remuda Browser GUI is missing from this installation. Install a nightly that includes the GUI."),
        Err(error) => return super::fail(format!("cannot locate Remuda Browser: {error}")),
    };
    match Command::new(&executable).args(args).spawn() {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => super::fail(format!(
            "cannot launch Remuda Browser at {}: {error}",
            executable.display()
        )),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn launch_gui(_: &[&str]) -> ExitCode {
    super::fail("The native Remuda Browser is supported on macOS and Windows")
}

fn publish_document(id: &str, uri: &str, session_only: bool) -> ExitCode {
    use remuda_core::protocol::{Request, Response};
    use remuda_native::net::browser::{
        Client, DocumentRequest, Operation, Outcome, Reply, DELIVERY_TTL,
    };
    use std::time::{Duration, Instant};

    let client = if session_only {
        None
    } else {
        match Client::from_env() {
            Ok(client) => client,
            Err(error) => return super::fail(format!("Browser publisher credentials: {error}")),
        }
    };
    let operation = Operation::Publish {
        request: DocumentRequest {
            id: id.into(),
            uri: uri.into(),
            origin: None,
        },
    };
    let request = Request::BrowserPublishDocument {
        id: id.into(),
        uri: uri.into(),
    };
    let mut origin = None;
    let socket = remuda_native::daemon::socket_path("default");
    let deadline = Instant::now() + DELIVERY_TTL + Duration::from_secs(2);
    let result = loop {
        let outcome = match &client {
            Some(client) => match client.send(&operation) {
                Ok(Reply::Outcome(outcome)) => Some(outcome),
                Ok(_) => None,
                Err(_) => Some(Outcome::Unknown),
            },
            None => match remuda_native::client::request_with_deadline(
                &socket,
                &request,
                Duration::from_secs(4),
            ) {
                Ok(Response::BrowserDocumentPublished {
                    origin: verified,
                    outcome,
                }) => {
                    origin = Some(verified);
                    Some(outcome)
                }
                Ok(_) => None,
                Err(_) => Some(Outcome::Unknown),
            },
        };
        match outcome {
            Some(Outcome::Pending) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Some(Outcome::Pending | Outcome::Unknown) => break "unknown",
            Some(Outcome::Displayed) => break "displayed",
            Some(Outcome::NoActiveBrowser) => break "no_active_browser",
            Some(Outcome::ReadFailed) => break "read_failed",
            Some(Outcome::Busy) => break "busy",
            Some(Outcome::ConflictingRequest) => break "conflicting_request",
            None => break "denied",
        }
    };
    let mut receipt = serde_json::json!({ "request_id": id, "uri": uri, "status": result });
    if let Some(origin) = origin {
        receipt["origin"] = serde_json::json!(origin);
    }
    println!("{receipt}");
    if result == "displayed" {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::valid_destination;

    #[test]
    fn ssh_destinations_are_single_hosts_or_config_aliases() {
        for destination in ["lab", "jane@dev.example.org", "127.0.0.1", "jane@::1"] {
            assert!(valid_destination(destination), "{destination}");
        }
        for destination in [
            "",
            "-F",
            "foo@-v",
            "a@b@c",
            "a b",
            "a\nb",
            "ssh://host",
            "a;id",
        ] {
            assert!(!valid_destination(destination), "{destination:?}");
        }
    }
}

// Only portable targets are emitted. In particular, file URLs could refer to
// a different file when the viewer is on the other end of an SSH connection.
fn terminal_link(uri: &str, label: &str, interactive: bool) -> Result<String, &'static str> {
    if label.is_empty() || label.len() > 2048 || label.chars().any(char::is_control) {
        return Err("Invalid terminal link label");
    }
    use remuda_native::terminal_link::Target;
    if !matches!(Target::parse(uri), Some(Target::Drive(_) | Target::Web(_))) {
        return Err("Terminal links require a Remuda Drive URI or an HTTP(S) URL");
    }

    if interactive {
        Ok(format!("\x1b]8;;{uri}\x1b\\{label}\x1b]8;;\x1b\\"))
    } else if label == uri {
        Ok(uri.to_owned())
    } else {
        Ok(format!("{label} ({uri})"))
    }
}

#[cfg(test)]
mod terminal_link_tests {
    use super::terminal_link;

    #[test]
    fn portable_links_emit_framed_osc_only_for_terminals() {
        let uri = "https://github.com/example/project/pull/42";
        assert_eq!(
            terminal_link(uri, "Review", true).unwrap(),
            "\x1b]8;;https://github.com/example/project/pull/42\x1b\\Review\x1b]8;;\x1b\\"
        );
        assert_eq!(
            terminal_link("remuda://team/docs/guide.md", "Guide", false).unwrap(),
            "Guide (remuda://team/docs/guide.md)"
        );
    }

    #[test]
    fn unsafe_targets_and_terminal_controls_are_rejected() {
        for uri in [
            "file:///tmp/guide.md",
            "javascript:alert(1)",
            "https://",
            "https://github.com@evil.invalid/project",
            "remuda:///../guide.md",
            "remuda:///guide.md\x1b\\",
        ] {
            assert!(terminal_link(uri, "Guide", true).is_err(), "{uri:?}");
        }
        assert!(terminal_link("remuda:///guide.md", "Guide\x07", true).is_err());
    }
}
