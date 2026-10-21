//! writer-fence-v1 over the private wire, and the frames older peers exchange.
use super::*;
use crate::session_host::api::{Writer, WRITER_FENCE};
use crate::session_host::entry::Client;
use portable_pty::CommandBuilder;
use remuda_core::Size;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

static NEXT: AtomicU64 = AtomicU64::new(0);
const STORE: [u8; 16] = [3; 16];

struct Fixture {
    root: PathBuf,
    host: Arc<Owned>,
    server: Arc<Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rm-fence-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let endpoint = Endpoint::bind(&root.join("host")).unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "while IFS= read -r line; do :; done"]);
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.cwd(&root);
        let host =
            Arc::new(Owned::spawn([6; 16], "fenced".into(), command, Size::new(80, 24)).unwrap());
        let server = Arc::new(Server::new(endpoint, host.clone()));
        let running = server.clone();
        let thread = Some(std::thread::spawn(move || running.run().unwrap()));
        Self {
            root,
            host,
            server,
            thread,
        }
    }
    fn control(&self) -> Client {
        let path = self.root.join("host");
        Client::open_expected(&path, Role::Control, self.host.identity()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(fence) = self.host.acquire(ConnectionId(u128::MAX)) {
            let _ = self.host.close(fence);
            let _ = self.host.release(fence);
        }
        self.server.stop_fixture();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn writer(epoch: u64) -> Writer {
    Writer {
        store: STORE,
        epoch,
    }
}
fn code(response: Response) -> String {
    match response {
        Response::Error { code } => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
fn resize(client: &mut Client) -> Response {
    client
        .request(&Request::Resize {
            rows: 30,
            cols: 100,
        })
        .unwrap()
}

#[test]
fn the_host_refuses_resize_close_and_stop_from_a_superseded_writer() {
    let fixture = Fixture::new();
    let mut old = fixture.control();
    assert!(matches!(
        old.acquire(writer(5)).unwrap(),
        Response::Granted { .. }
    ));
    assert!(matches!(resize(&mut old), Response::Ok));
    let mut new = fixture.control();
    assert_eq!(code(new.acquire(writer(6)).unwrap()), "controller_busy");
    // The lease is still held, yet every effect of the older writer is fenced.
    assert_eq!(code(resize(&mut old)), "superseded_writer");
    assert_eq!(
        code(old.request(&Request::Close).unwrap()),
        "superseded_writer"
    );
    assert_eq!(
        code(old.request(&Request::Stop).unwrap()),
        "superseded_writer"
    );
    assert!(matches!(
        old.request(&Request::Release).unwrap(),
        Response::Ok
    ));
    assert!(matches!(
        new.acquire(writer(6)).unwrap(),
        Response::Granted { .. }
    ));
    assert!(matches!(resize(&mut new), Response::Ok));
    drop(new);
    assert_eq!(
        code(fixture.control().acquire(writer(5)).unwrap()),
        "superseded_writer"
    );
    // Negative control: an unfenced Acquire, as an older daemon sends, is not refused.
    let mut plain = fixture.control();
    let granted = plain.request(&Request::Acquire).unwrap();
    assert!(matches!(granted, Response::Granted { .. }));
}

/// Frames as an older peer encodes them: N/N+1 compatibility at the wire.
#[test]
fn older_peers_frames_are_unchanged_and_still_accepted() {
    let acquire = serde_json::to_string(&Request::Acquire).unwrap();
    assert_eq!(acquire, r#"{"op":"Acquire"}"#);
    let fenced = serde_json::to_string(&Request::AcquireWriter { writer: writer(2) }).unwrap();
    let decoded: Request = serde_json::from_str(&fenced).unwrap();
    assert!(matches!(decoded, Request::AcquireWriter { writer: w } if w == writer(2)));
    // deny_unknown_fields does not cover a unit variant: a field added to
    // Acquire would be silently dropped by an older host, so fencing is a
    // new variant behind a capability. An unknown variant is refused.
    let widened = r#"{"op":"Acquire","writer":{"store":[0],"epoch":1}}"#;
    assert!(serde_json::from_str::<Request>(widened).is_ok());
    let unknown = r#"{"op":"AcquireNext"}"#;
    assert!(serde_json::from_str::<Request>(unknown).is_err());
    // An older client accepts at most 16 capabilities of at most 64 bytes.
    let mut caps = CAPS.to_vec();
    caps.push(super::super::api::SCOPED_BIRTH);
    assert!(caps.contains(&WRITER_FENCE));
    assert!(caps.len() <= 16 && caps.iter().all(|c| c.len() <= 64));
    // An older daemon's control Hello, captured verbatim, still opens.
    let fixture = Fixture::new();
    let identity = serde_json::to_value(fixture.host.identity()).unwrap();
    let open = serde_json::json!({
        "protocol": {"magic": [82, 77, 72, 83], "major": 1, "minor": 0,
                     "required": ["inspect", "controller-epoch-v1", "input-act-v1"]},
        "role": "Control",
        "expected": identity,
    });
    let mut channel = super::super::unix::connect(&fixture.root.join("host")).unwrap();
    channel.send(&serde_json::to_vec(&open).unwrap()).unwrap();
    let opened: Response = serde_json::from_slice(&channel.receive().unwrap()).unwrap();
    assert!(matches!(opened, Response::Opened { minor: 0, .. }));
    channel.send(acquire.as_bytes()).unwrap();
    let granted: Response = serde_json::from_slice(&channel.receive().unwrap()).unwrap();
    assert!(matches!(granted, Response::Granted { .. }));
}
