//! What the session screen asks of the node it shows. The local daemon socket
//! (`Path`) is the only port today; a cluster peer is meant to be the next.
use crate::client::{self, Hold};
use remuda_core::protocol::{Request, Response};
use std::io;
use std::path::Path;

/// Live keys for the focused session; a local attach is the first kind.
pub trait KeyStream {
    fn keys(&self, bytes: &[u8]) -> io::Result<()>;
}

impl KeyStream for Hold {
    fn keys(&self, bytes: &[u8]) -> io::Result<()> {
        Hold::keys(self, bytes)
    }
}

pub trait SessionPort {
    type Keys: KeyStream;

    fn request(&self, request: &Request) -> io::Result<Response>;

    /// Take the session's live key stream; a refusal says why.
    fn hold(&self, name: &str) -> io::Result<Self::Keys>;

    /// Whether the node answers at all, once a request has failed.
    fn probe(&self) -> io::Result<()>;

    /// For a failed probe: (the node is definitively gone, a busy node refused).
    fn probe_verdict(&self, error: &io::Error) -> (bool, bool);

    /// Where the node lives, as shown when it is gone.
    fn label(&self) -> String;

    /// False when the node's Lua cannot be asked for list and window layout;
    /// the screen then shows plain names and the selected session.
    fn has_lua(&self) -> bool {
        true
    }

    /// True when `r` may restart this node's daemon from here.
    fn restartable(&self) -> bool {
        true
    }

    /// False when `m` lines cannot be sent to this node yet; `m` then says so.
    fn accepts_lines(&self) -> bool {
        true
    }
}

impl SessionPort for Path {
    type Keys = Hold;

    fn request(&self, request: &Request) -> io::Result<Response> {
        client::request(self, request)
    }

    fn hold(&self, name: &str) -> io::Result<Hold> {
        client::hold(self, name)
    }

    fn probe(&self) -> io::Result<()> {
        crate::ipc::connect(self).map(|_| ())
    }

    fn probe_verdict(&self, error: &io::Error) -> (bool, bool) {
        let definitive = super::daemon_is_definitively_gone(self, error);
        let busy_refusal = error.kind() == io::ErrorKind::ConnectionRefused
            && self.exists()
            && !super::socket_lock_is_free(self);
        (definitive, busy_refusal)
    }

    fn label(&self) -> String {
        self.display().to_string()
    }
}

impl SessionPort for std::path::PathBuf {
    type Keys = Hold;

    fn request(&self, request: &Request) -> io::Result<Response> {
        self.as_path().request(request)
    }

    fn hold(&self, name: &str) -> io::Result<Hold> {
        self.as_path().hold(name)
    }

    fn probe(&self) -> io::Result<()> {
        self.as_path().probe()
    }

    fn probe_verdict(&self, error: &io::Error) -> (bool, bool) {
        self.as_path().probe_verdict(error)
    }

    fn label(&self) -> String {
        self.as_path().label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A node that is not a daemon socket: records requests, answers from a script.
    struct FakePort {
        sent: Mutex<Vec<Request>>,
        keys: std::sync::Arc<Mutex<Vec<u8>>>,
    }

    struct FakeKeys(std::sync::Arc<Mutex<Vec<u8>>>);

    impl KeyStream for FakeKeys {
        fn keys(&self, bytes: &[u8]) -> io::Result<()> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }
    }

    impl SessionPort for FakePort {
        type Keys = FakeKeys;

        fn request(&self, request: &Request) -> io::Result<Response> {
            self.sent.lock().unwrap().push(request.clone());
            Ok(match request {
                Request::List => Response::Sessions(Vec::new()),
                _ => Response::Ok,
            })
        }

        fn hold(&self, _name: &str) -> io::Result<FakeKeys> {
            Ok(FakeKeys(self.keys.clone()))
        }

        fn probe(&self) -> io::Result<()> {
            Ok(())
        }

        fn probe_verdict(&self, _error: &io::Error) -> (bool, bool) {
            (false, false)
        }

        fn label(&self) -> String {
            "fake".into()
        }
    }

    /// A peer-like node: no Lua, one live session whose screen says "hi".
    struct NoLuaPort(Mutex<Vec<Request>>);

    impl SessionPort for NoLuaPort {
        type Keys = FakeKeys;

        fn request(&self, request: &Request) -> io::Result<Response> {
            self.0.lock().unwrap().push(request.clone());
            Ok(match request {
                Request::List => Response::Sessions(vec![remuda_core::SessionSummary {
                    id: "id".into(),
                    name: "build".into(),
                    instance_id: Some("i".into()),
                    output_version: Some(1),
                    alive: true,
                    idle: std::time::Duration::ZERO,
                    output_idle: None,
                    size: remuda_core::Size::new(80, 24),
                    attached: false,
                    human_idle: None,
                    mouse_tracking: false,
                    unavailable: false,
                }]),
                Request::CaptureStyled { .. } => Response::StyledScreen {
                    rows: vec![remuda_core::protocol::collapse_runs(&[
                        remuda_core::agent::StyledCell {
                            text: "hi".into(),
                            ..Default::default()
                        },
                    ])],
                    instance_id: Some("i".into()),
                    output_version: Some(1),
                    wrapped: vec![false],
                    scrollback_len: 0,
                    scrollback_total: 0,
                    cursor: remuda_core::agent::Cursor {
                        row: 0,
                        col: 0,
                        visible: false,
                    },
                },
                other => Response::Error(format!("refused {other:?}")),
            })
        }

        fn hold(&self, _name: &str) -> io::Result<FakeKeys> {
            Err(io::Error::other("no keys"))
        }

        fn probe(&self) -> io::Result<()> {
            Ok(())
        }

        fn probe_verdict(&self, _error: &io::Error) -> (bool, bool) {
            (false, false)
        }

        fn label(&self) -> String {
            "node peer".into()
        }

        fn has_lua(&self) -> bool {
            false
        }

        fn restartable(&self) -> bool {
            false
        }
    }

    #[test]
    fn a_node_without_lua_gets_plain_names_and_its_selected_screen() {
        let port = NoLuaPort(Mutex::new(Vec::new()));
        let mut ui = super::super::Ui::new(Vec::new(), "sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;
        super::super::refresh(
            &port,
            "peer",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(ui.sessions.len(), 1);
        assert!(ui.sessions_text.is_empty(), "plain names, no Lua buffer");
        assert!(
            matches!(&shown, Some(super::super::ShownTarget::Session(name)) if name == "build")
        );
        assert!(painted.contains("build"));
        assert!(painted.contains("hi"));
        let sent = port.0.lock().unwrap();
        assert!(
            !sent
                .iter()
                .any(|request| matches!(request, Request::Eval { .. })),
            "a peer refuses Eval, so the screen must not ask: {sent:?}"
        );
    }

    #[test]
    fn session_screen_helpers_run_over_any_port() {
        let port = FakePort {
            sent: Mutex::new(Vec::new()),
            keys: Default::default(),
        };
        assert_eq!(super::super::list(&port).unwrap(), Vec::new());
        super::super::kill(&port, "dev").unwrap();
        super::super::resize(&port, "dev", remuda_core::Size::new(80, 24)).unwrap();
        let sent = port.sent.lock().unwrap();
        assert!(matches!(sent[0], Request::List));
        assert!(matches!(&sent[1], Request::Close { name, .. } if name == "dev"));
        assert!(matches!(&sent[2], Request::Resize { name, .. } if name == "dev"));
        drop(sent);
        port.hold("dev").unwrap().keys(b"ls\r").unwrap();
        assert_eq!(*port.keys.lock().unwrap(), b"ls\r");
    }
}
