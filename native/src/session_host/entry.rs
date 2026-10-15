//! Explicit experimental executable entry; never called by daemon startup.
use super::api::{Manifest, Open, Request, Response, Role};
use super::channel::Channel;
use super::owned::{Owned, Staged};
use super::server::Server;
use super::unix::Endpoint;
use super::wire::{Hello, MAGIC, MAJOR, MAX_FRAME};
use portable_pty::CommandBuilder;
use remuda_core::Size;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Launch {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub agent: String,
    pub rows: u16,
    pub cols: u16,
    pub env: Vec<(String, String)>,
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}
fn random() -> io::Result<[u8; 16]> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| invalid("host entropy unavailable"))?;
    Ok(bytes)
}
pub(crate) fn run(path: &Path) -> io::Result<()> {
    let bytes = super::wire::read_frame(&mut io::stdin().lock(), MAX_FRAME)?;
    let launch: Launch = serde_json::from_slice(&bytes).map_err(invalid)?;
    validate(&launch)?;
    // A host has its own process session; inherited bootstrap stdin closing
    // does not terminate the host or its child. No parent-death guard is used.
    if unsafe { libc::getsid(0) } != unsafe { libc::getpid() } && unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    let endpoint = Endpoint::bind(path)?;
    endpoint.nonblocking()?;
    let mut command = CommandBuilder::new(&launch.argv[0]);
    command.args(&launch.argv[1..]);
    command.cwd(&launch.cwd);
    command.env_clear();
    for (key, value) in &launch.env {
        command.env(key, value);
    }
    let staged = Staged::new(
        Owned::spawn(
            random()?,
            launch.name.clone(),
            command,
            Size::new(launch.cols, launch.rows),
        )
        .map_err(invalid)?,
    );
    let manifest = Manifest {
        schema: 1,
        identity: staged.host().identity(),
        name: launch.name,
        agent: launch.agent,
        cwd: launch.cwd,
    };
    let bytes = serde_json::to_vec(&manifest).map_err(invalid)?;
    if bytes.len() > 8192 {
        return Err(invalid("host manifest byte budget"));
    }
    crate::fs_atomic::write_atomic_lua_private(&path.join("session.json"), &bytes)?;
    let host = staged.publish();
    let server = Arc::new(Server::new(endpoint, host));
    // Commit precedes acknowledgement. A dead/stalled launcher must not kill
    // or block this discoverable host. At most one bounded (8 KiB) ACK task
    // exists; it holds no Session/parser/inventory lock.
    let _ = std::thread::Builder::new()
        .name("session-host-launch-ack".into())
        .spawn(move || {
            let mut output = io::stdout().lock();
            let _ = super::wire::write_frame(&mut output, &bytes).and_then(|()| output.flush());
        });
    server.run()
}
fn validate(launch: &Launch) -> io::Result<()> {
    if launch.name.is_empty()
        || launch.name.len() > 64
        || launch.name.chars().any(char::is_control)
        || launch.argv.is_empty()
        || launch.argv.len() > 32
        || launch.argv.iter().map(String::len).sum::<usize>() > 16 * 1024
        || launch.argv[0].is_empty()
        || launch.agent.len() > 32
        || launch.cwd.len() > 1024
        || !Path::new(&launch.cwd).is_absolute()
        || launch.env.len() > 64
        || launch.env.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > 128
                || value.len() > 4096
                || key.starts_with("REMUDA_SESSION_HOST_")
        })
        || launch
            .env
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
            > 16 * 1024
        || launch.rows < 24
        || launch.cols < 80
    {
        return Err(invalid("invalid or oversized experimental host launch"));
    }
    super::terminal::Terminal::validate_geometry(launch.rows, launch.cols, 1000).map_err(invalid)
}
pub(crate) fn manifest(path: &Path) -> io::Result<Manifest> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("session.json"))?;
    let info = file.metadata()?;
    if !info.is_file()
        || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & 0o7777 != 0o600
        || info.nlink() != 1
        || info.len() > 8192
    {
        return Err(invalid("private host manifest validation failed"));
    }
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(invalid("host manifest limit"));
    }
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(invalid)?;
    if manifest.schema != 1 {
        return Err(invalid("unsupported host manifest schema"));
    }
    Ok(manifest)
}
pub(crate) struct Client {
    channel: Channel<std::os::unix::net::UnixStream>,
}
impl Client {
    pub fn open(path: &Path, role: Role) -> io::Result<Self> {
        // Expected intent comes from private local metadata, not server echo.
        let manifest = manifest(path)?;
        let mut channel = super::unix::connect(path)?;
        let open = Open {
            protocol: Hello {
                magic: MAGIC,
                major: MAJOR,
                minor: 0,
                required: vec!["inspect".into()],
            },
            role,
            expected: Some(manifest.identity.clone()),
        };
        channel.send(&serde_json::to_vec(&open).map_err(invalid)?)?;
        match serde_json::from_slice::<Response>(&channel.receive()?).map_err(invalid)? {
            Response::Opened { identity, minor: 0 } if identity == manifest.identity => {
                Ok(Self { channel })
            }
            _ => Err(invalid("host identity/protocol refusal")),
        }
    }
    pub fn request(&mut self, request: &Request) -> io::Result<Response> {
        self.channel
            .send(&serde_json::to_vec(request).map_err(invalid)?)?;
        serde_json::from_slice(&self.channel.receive()?).map_err(invalid)
    }
}
pub(crate) fn command(path: &Path, operation: &str) -> io::Result<()> {
    let control = matches!(
        operation,
        "close" | "stop" | "hold" | "hold-input" | "rpc-control" | "rpc-session-control"
    );
    let mut client = Client::open(path, if control { Role::Control } else { Role::View })?;
    if control && !matches!(client.request(&Request::Acquire)?, Response::Granted { .. }) {
        return Err(invalid("controller acquisition refused"));
    }
    let response = match operation {
        "inspect" => client.request(&Request::Inspect)?,
        "rpc-session-view" | "rpc-session-control" => {
            let mut input = io::stdin().lock();
            loop {
                let bytes = match super::wire::read_frame(&mut input, MAX_FRAME) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(error) => return Err(error),
                };
                let request: Request = serde_json::from_slice(&bytes).map_err(invalid)?;
                let response = client.request(&request)?;
                serde_json::to_writer(io::stdout().lock(), &response).map_err(invalid)?;
                println!();
                io::stdout().flush()?;
            }
        }
        "rpc-view" | "rpc-control" => {
            let bytes = super::wire::read_frame(&mut io::stdin().lock(), MAX_FRAME)?;
            let request: Request = serde_json::from_slice(&bytes).map_err(invalid)?;
            client.request(&request)?
        }
        "screen" => {
            let (generation, bytes, rows, cols) = screen(&mut client)?;
            use base64::Engine;
            serde_json::to_writer(io::stdout().lock(), &serde_json::json!({"generation": generation, "rows": rows, "cols": cols, "screen_base64": base64::engine::general_purpose::STANDARD.encode(bytes)})).map_err(invalid)?;
            println!();
            return Ok(());
        }
        "hold-input" => {
            let bytes = super::wire::read_frame(&mut io::stdin().lock(), MAX_FRAME)?;
            let request: Request = serde_json::from_slice(&bytes).map_err(invalid)?;
            if !matches!(request, Request::Input { .. }) {
                return Err(invalid("hold-input requires an Input request"));
            }
            let admitted = client.request(&request)?;
            serde_json::to_writer(io::stdout().lock(), &admitted).map_err(invalid)?;
            println!();
            io::stdout().flush()?;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                client.request(&Request::Inspect)?;
            }
        }
        "close" => client.request(&Request::Close)?,
        "stop" => client.request(&Request::Stop)?,
        "hold" => {
            println!("controller-acquired");
            io::stdout().flush()?;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                client.request(&Request::Inspect)?;
            }
        }
        _ => return Err(invalid("expected inspect/close/stop/hold")),
    };
    serde_json::to_writer(io::stdout().lock(), &response).map_err(invalid)?;
    println!();
    Ok(())
}

fn screen(client: &mut Client) -> io::Result<(u64, Vec<u8>, u16, u16)> {
    use base64::Engine;
    let Response::Snapshot {
        snapshot,
        generation,
        bytes,
        pages,
        rows,
        cols,
    } = client.request(&Request::Snapshot)?
    else {
        return Err(invalid("snapshot refused; resync required"));
    };
    super::terminal::Terminal::validate_geometry(rows, cols, 1000).map_err(invalid)?;
    const PAGE: usize = 12 * 1024;
    if bytes > 32 * 1024 * 1024 || pages != bytes.div_ceil(PAGE) {
        return Err(invalid("snapshot allocation budget"));
    }
    let mut assembled = Vec::with_capacity(bytes);
    for index in 0..pages {
        let Response::Page {
            snapshot: reply,
            index: page,
            base64,
        } = client.request(&Request::Page { snapshot, index })?
        else {
            return Err(invalid("snapshot expired; resync required"));
        };
        if reply != snapshot || page != index || base64.len() > PAGE * 4 / 3 {
            return Err(invalid("snapshot page identity/limit"));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(base64)
            .map_err(invalid)?;
        if data.len() != (bytes - assembled.len()).min(PAGE) {
            return Err(invalid("snapshot page length"));
        }
        assembled.extend_from_slice(&data);
    }
    if !matches!(
        client.request(&Request::Acknowledge {
            snapshot,
            generation
        })?,
        Response::Ok
    ) {
        return Err(invalid("snapshot baseline expired"));
    }
    Ok((generation, assembled, rows, cols))
}
