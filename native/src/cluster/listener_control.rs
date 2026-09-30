//! Callable operations over the daemon-owned cluster listener.

use super::listener_config::{self, ListenerBind, ListenerConfig};
use remuda_core::protocol::{ListenerOp, ListenerStatus, Request, Response};
use std::io;
use std::path::Path;

/// Read the persisted listener configuration.
pub fn config() -> io::Result<Option<ListenerConfig>> {
    listener_config::read()
}

/// Read the current status from the daemon at `daemon_path`.
pub fn status(daemon_path: &Path) -> ListenerStatus {
    match request(daemon_path, ListenerOp::Status) {
        Ok(status) => status,
        Err(error) => ListenerStatus::Failed(error.to_string()),
    }
}

/// Enable the listener and ask the selected daemon to reload its task.
pub fn start(daemon_path: &Path, config: Option<ListenerConfig>) -> io::Result<ListenerStatus> {
    let mut config = match config {
        Some(config) => config,
        None => listener_config::read()?.unwrap_or(ListenerConfig {
            enabled: true,
            bind: ListenerBind::Auto,
            allow_public: false,
        }),
    };
    config.enabled = true;
    listener_config::write(&config)?;
    request(daemon_path, ListenerOp::Reload)
}

/// Disable the listener and ask the selected daemon to reload its task.
pub fn stop(daemon_path: &Path) -> io::Result<ListenerStatus> {
    let mut config = listener_config::read()?.unwrap_or(ListenerConfig {
        enabled: false,
        bind: ListenerBind::Auto,
        allow_public: false,
    });
    config.enabled = false;
    config.allow_public = false;
    listener_config::write(&config)?;
    request(daemon_path, ListenerOp::Reload)
}

/// Restore a previously saved listener config and ask the daemon to reload it.
pub fn restore(daemon_path: &Path, snapshot: Option<ListenerConfig>) -> io::Result<ListenerStatus> {
    match snapshot {
        Some(config) => listener_config::write(&config)?,
        None => listener_config::remove()?,
    }
    request(daemon_path, ListenerOp::Reload)
}

fn request(daemon_path: &Path, operation: ListenerOp) -> io::Result<ListenerStatus> {
    match crate::client::request(daemon_path, &Request::ClusterListener(operation))? {
        Response::ClusterListenerStatus(status) => Ok(status),
        Response::Error(reason) => Err(io::Error::other(reason)),
        response => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected cluster listener response: {response:?}"),
        )),
    }
}
