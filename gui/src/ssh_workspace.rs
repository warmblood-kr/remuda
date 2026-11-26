//! A single authenticated SSH connection to the remote workspace helper.

use remuda_core::protocol::{Request, Response};
use remuda_native::{
    net::browser::valid_ssh_destination,
    remote_front::{
        MAX_FRAME_BYTES, MAX_WORKSPACE_RESPONSE_BYTES, WORKSPACE_COMMAND, WORKSPACE_HELLO,
    },
    strict_input::InputTransport,
};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Instant,
};

struct Job {
    request: Request,
    cancelled: Option<Arc<AtomicBool>>,
    deadline: Instant,
    reply: mpsc::SyncSender<io::Result<Response>>,
}

struct Connection {
    child: Arc<Mutex<Child>>,
    jobs: mpsc::SyncSender<Job>,
}

impl Connection {
    fn abort(&self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Clone)]
pub(crate) struct SshWorkspace {
    connection: Arc<Connection>,
    destination: Arc<str>,
}

impl SshWorkspace {
    pub(crate) fn connect(destination: &str) -> io::Result<Self> {
        if !valid_ssh_destination(destination) {
            return Err(io::Error::other("invalid SSH destination"));
        }
        let mut command = Command::new("ssh");
        command
            .args([
                "-T",
                "-a",
                "-x",
                "-S",
                "none",
                "-o",
                "SendEnv=-*",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "BatchMode=yes",
                "--",
                destination,
                WORKSPACE_COMMAND,
            ])
            .env_remove("REMUDA_BROWSER_CLIENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let child = Arc::new(Mutex::new(child));
        let (jobs, incoming) = mpsc::sync_channel::<Job>(3);
        let connection = Arc::new(Connection {
            child: child.clone(),
            jobs,
        });
        std::thread::Builder::new()
            .name("gui-ssh-workspace".into())
            .spawn(move || {
                if frame(&mut stdout, WORKSPACE_HELLO.len())
                    .is_ok_and(|hello| hello == WORKSPACE_HELLO)
                {
                    for job in incoming {
                        if Instant::now() >= job.deadline {
                            let _ = job.reply.send(Err(io::Error::other("SSH request expired")));
                            continue;
                        }
                        if job
                            .cancelled
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            let _ = job.reply.send(Err(io::Error::other("SSH input detached")));
                            continue;
                        }
                        let result = exchange(&mut stdin, &mut stdout, &job.request);
                        let failed = result.is_err();
                        let _ = job.reply.send(result);
                        if failed {
                            break;
                        }
                    }
                }
                let mut child = child.lock().unwrap();
                let _ = child.kill();
                let _ = child.wait();
            })?;
        let workspace = Self {
            connection,
            destination: destination.into(),
        };
        // Probe an existing daemon. A greeting alone only proves remote CLI support.
        match workspace.request(&Request::List)? {
            Response::Sessions(_) => Ok(workspace),
            _ => Err(io::Error::other("SSH workspace session list unavailable")),
        }
    }

    pub(crate) fn destination(&self) -> &str {
        &self.destination
    }

    pub(crate) fn sessions(&self) -> Result<Vec<crate::Session>, String> {
        let rows = match self.request(&Request::List) {
            Ok(Response::Sessions(rows)) => rows,
            _ => return Err("SSH workspace session list unavailable".into()),
        };
        let view = match self.request(&Request::RenderedSessionList {
            width: crate::SESSION_VIEW_WIDTH,
        }) {
            Ok(Response::RenderedSessionList(view)) => Some(view),
            _ => None,
        };
        Ok(crate::session_rows(rows, view))
    }

    pub(crate) fn preview(
        &self,
        selection: &crate::Selection,
        resize: Option<remuda_core::agent::Size>,
        scroll: &str,
    ) -> Result<crate::Screen, String> {
        use remuda_native::session_preview::{capture, IdentityPolicy};
        if selection.peer.is_some() {
            return Err("SSH workspace session is outside the gateway".into());
        }
        let resize_refused = resize.is_some_and(|size| {
            !matches!(
                self.request(&Request::ResizeSession {
                    name: selection.name.clone(),
                    instance_id: selection.instance.clone(),
                    size,
                }),
                Ok(Response::Ok)
            )
        });
        let mut screen = capture(
            self,
            &selection.name,
            Some(&selection.instance),
            scroll
                .parse::<usize>()
                .map_err(|_| "invalid preview scroll")?
                .min(10_000),
            IdentityPolicy::Strict,
        )
        .and_then(crate::preview::styled)?;
        if screen.text.len() > 65_536 {
            return Err("SSH workspace preview is too large".into());
        }
        screen.resize_refused = resize_refused;
        Ok(screen)
    }

    fn request(&self, request: &Request) -> io::Result<Response> {
        self.dispatch(request, None)
    }

    fn dispatch(
        &self,
        request: &Request,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> io::Result<Response> {
        let (reply, result) = mpsc::sync_channel(1);
        let timeout = crate::provider::TIMEOUT;
        self.connection
            .jobs
            .try_send(Job {
                request: request.clone(),
                cancelled,
                deadline: Instant::now() + timeout,
                reply,
            })
            .map_err(|_| io::Error::other("SSH workspace unavailable or busy"))?;
        match result.recv_timeout(timeout) {
            Ok(value) => value,
            Err(_) => {
                // A reply might have been lost after execution. Close the entire
                // stream and leave any reconnection to an explicit new launch.
                self.connection.abort();
                Err(io::Error::other("SSH workspace did not answer"))
            }
        }
    }
}

pub(crate) struct SshInput {
    pub(crate) workspace: SshWorkspace,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl InputTransport for SshInput {
    fn send(&mut self, request: &Request) -> io::Result<Response> {
        self.workspace
            .dispatch(request, Some(self.cancelled.clone()))
    }
}

fn frame(input: &mut impl BufRead, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if bytes.len() > limit || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid SSH workspace frame"));
    }
    Ok(bytes)
}

fn exchange(
    output: &mut impl Write,
    input: &mut impl BufRead,
    request: &Request,
) -> io::Result<Response> {
    let json = serde_json::to_vec(request)?;
    if json.len() > MAX_FRAME_BYTES {
        return Err(io::Error::other("SSH workspace request too large"));
    }
    output.write_all(&json)?;
    output.write_all(b"\n")?;
    output.flush()?;
    serde_json::from_slice(&frame(input, MAX_WORKSPACE_RESPONSE_BYTES + 1)?)
        .map_err(|_| io::Error::other("invalid SSH workspace reply"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an isolated OpenSSH fixture with a pinned host key and one running session"]
    fn isolated_ssh_workspace_reads_previews_and_sends_instance_bound_input() {
        use remuda_native::strict_input::{InputResult, StrictInput};
        let destination = std::env::var("REMUDA_SSH_WORKSPACE_FIXTURE").unwrap();
        let workspace = SshWorkspace::connect(&destination).unwrap();
        let rows = workspace.sessions().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(row.alive);
        let mut selection = crate::Selection {
            peer: None,
            name: row.name.clone(),
            instance: "obsolete-instance".into(),
        };
        assert!(workspace.preview(&selection, None, "0").is_err());
        selection.instance = row.instance_id.clone().unwrap();
        let size = remuda_core::agent::Size::for_viewport(93, 35);
        let screen = workspace.preview(&selection, Some(size), "0").unwrap();
        assert!(!screen.resize_refused);
        let transport = SshInput {
            workspace: workspace.clone(),
            cancelled: Arc::new(AtomicBool::new(true)),
        };
        let mut detached = StrictInput::new(
            transport,
            selection.name.clone(),
            selection.instance.clone(),
        )
        .unwrap();
        assert_eq!(detached.send(b"x"), InputResult::NotAcknowledged);
        let transport = SshInput {
            workspace,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let mut input = StrictInput::new(transport, selection.name, selection.instance).unwrap();
        assert_eq!(input.send(b"x"), InputResult::Applied);
    }

    #[test]
    fn reply_frames_require_newline_and_respect_bound() {
        assert_eq!(frame(&mut &b"abc\nrest"[..], 4).unwrap(), b"abc\n");
        assert!(frame(&mut &b"abc"[..], 4).is_err());
        assert!(frame(&mut &b"abcd\n"[..], 4).is_err());
    }
}

impl remuda_native::tui::SessionPort for SshWorkspace {
    type Keys = remuda_native::client::Hold;

    fn request(&self, request: &Request) -> io::Result<Response> {
        SshWorkspace::request(self, request)
    }

    fn hold(&self, _: &str) -> io::Result<Self::Keys> {
        Err(io::Error::other("SSH workspace uses instance-bound input"))
    }

    fn probe(&self) -> io::Result<()> {
        match self.request(&Request::List)? {
            Response::Sessions(_) => Ok(()),
            _ => Err(io::Error::other("SSH workspace session list unavailable")),
        }
    }

    fn probe_verdict(&self, _: &io::Error) -> (bool, bool) {
        (true, false)
    }

    fn label(&self) -> String {
        self.destination.to_string()
    }

    fn remote_node(&self) -> Option<String> {
        Some(self.destination.to_string())
    }
}
