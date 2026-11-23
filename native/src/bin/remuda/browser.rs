use std::io::{self, IsTerminal};
use std::process::{Command, ExitCode};

const USAGE: &str =
    "Usage: remuda browser [USER@]HOST\n       remuda browser open-document REQUEST_ID VFS_URI";

pub(super) fn run(args: &[&str], server: &str, stdin_enabled: bool) -> ExitCode {
    if matches!(args, ["-h" | "--help" | "help"]) {
        println!(
            "{USAGE}\n\nOpen the remote Remuda TUI through SSH. An SSH config alias is also accepted.\nThe remote login account must have remuda on its PATH."
        );
        return ExitCode::SUCCESS;
    }
    if server != "default" || stdin_enabled {
        return super::fail(format!(
            "{USAGE}\nLocal --server and --stdin options are unsupported."
        ));
    }
    if let ["open-document", request_id, uri] = args {
        return publish_document(request_id, uri);
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
        .args(["-a", "-x", "-o", "ClearAllForwardings=yes", "-t", "--"])
        .arg(destination)
        .arg("remuda")
        .status()
    {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => super::fail(format!("remuda browser: could not run ssh: {error}")),
    }
}

fn publish_document(id: &str, uri: &str) -> ExitCode {
    use remuda_native::net::browser::{Client, DocumentRequest, Operation, Outcome, Reply};
    use std::time::{Duration, Instant};

    let client = match Client::from_env() {
        Ok(Some(client)) => client,
        Ok(None) => {
            return super::fail("REMUDA_BROWSER_CLIENT must identify the publisher credentials")
        }
        Err(error) => return super::fail(format!("Browser publisher credentials: {error}")),
    };
    let operation = Operation::Publish {
        request: DocumentRequest {
            id: id.into(),
            uri: uri.into(),
        },
    };
    let deadline = Instant::now() + Duration::from_secs(17);
    let result = loop {
        match client.send(&operation) {
            Ok(Reply::Outcome(Outcome::Pending)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Ok(Reply::Outcome(Outcome::Pending)) | Err(_) => break "unknown",
            Ok(Reply::Outcome(Outcome::Displayed)) => break "displayed",
            Ok(Reply::Outcome(Outcome::NoActiveBrowser)) => break "no_active_browser",
            Ok(Reply::Outcome(Outcome::Unknown)) => break "unknown",
            Ok(Reply::Outcome(Outcome::ReadFailed)) => break "read_failed",
            Ok(Reply::Outcome(Outcome::Busy)) => break "busy",
            Ok(Reply::Outcome(Outcome::ConflictingRequest)) => break "conflicting_request",
            Ok(_) => break "denied",
        }
    };
    println!(
        "{}",
        serde_json::json!({ "request_id": id, "uri": uri, "status": result })
    );
    if result == "displayed" {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn valid_destination(destination: &str) -> bool {
    fn name(value: &str) -> bool {
        value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    }
    let host = match destination.split_once('@') {
        Some((user, host)) if name(user) => host,
        Some(_) => return false,
        None => destination,
    };
    name(host) || host.parse::<std::net::Ipv6Addr>().is_ok()
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
