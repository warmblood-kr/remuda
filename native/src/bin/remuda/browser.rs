use remuda_native::net::browser::{ssh_arguments, valid_ssh_destination as valid_destination};
use std::io::{self, IsTerminal};
use std::process::{Command, ExitCode};

const USAGE: &str =
    "Usage: remuda browser [USER@]HOST\n       remuda browser --gui [USER@]HOST\n       remuda browser open-document REQUEST_ID VFS_URI";

pub(super) fn run(args: &[&str], server: &str, stdin_enabled: bool) -> ExitCode {
    if matches!(args, ["-h" | "--help" | "help"]) {
        println!(
            "{USAGE}\n\nConnect to Remuda through SSH. Use --gui for the native client on macOS or Windows.\nAn SSH config alias is accepted. The remote login account must have remuda on its PATH."
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
    if let ["--gui", destination] = args {
        if !valid_destination(destination) {
            return super::fail(USAGE);
        }
        return launch_gui(destination);
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
fn launch_gui(destination: &str) -> ExitCode {
    let cli = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => return super::fail(format!("cannot locate Remuda Browser: {error}")),
    };
    let executable = match super::gui_managed::choose(&cli, remuda_native::dist::BUILD_VERSION) {
        Ok(Some(path)) => path,
        Ok(None) => return super::fail("Remuda Browser GUI is missing from this installation. Install a nightly that includes the GUI."),
        Err(error) => return super::fail(format!("cannot locate Remuda Browser: {error}")),
    };
    match Command::new(&executable)
        .args(["--browser", destination])
        .spawn()
    {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => super::fail(format!(
            "cannot launch Remuda Browser at {}: {error}",
            executable.display()
        )),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn launch_gui(_: &str) -> ExitCode {
    super::fail("The native Remuda Browser is supported on macOS and Windows")
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
