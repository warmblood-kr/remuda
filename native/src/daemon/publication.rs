//! Staged endpoint publication for `serve_inner`: the listener is bound and
//! receipted on a private name, then published by exclusive rename.

use super::*;
use crate::staged_socket::{
    owner::Owner,
    startup::{self, Controls, Failure},
};

/// A published listener and the lifecycle owner holding its receipt.
pub(super) struct Imported {
    pub listener: Listener,
    pub owner: Arc<SocketOwnership>,
    pub residue: Vec<String>,
}

/// Primary failure plus every entry left behind.
#[derive(Debug)]
pub(super) struct StagedError {
    pub error: std::io::Error,
    pub residue: Vec<String>,
}

impl From<std::io::Error> for StagedError {
    fn from(error: std::io::Error) -> Self {
        Self {
            error,
            residue: Vec::new(),
        }
    }
}

pub(super) fn bind_staged(path: &Path, controls: &Controls) -> Result<Imported, StagedError> {
    let opened = startup::open(path, controls).map_err(|e| StagedError {
        error: e.error,
        residue: e.residue,
    })?;
    let published = opened.published;
    let mut residue = opened.residue;
    let listener = Listener::UdSocket(opened.listener.into());
    let imported = if controls.fails(Failure::Import) || controls.fails(Failure::ImportCleanup) {
        Err(std::io::Error::other("injected import failure"))
    } else {
        listener.set_nonblocking(ListenerNonblockingMode::Accept)
    };
    if let Err(error) = imported {
        drop(listener);
        if controls.fails(Failure::ImportCleanup) {
            residue.push(format!(
                "published socket {}: injected published unlink failure",
                path.display()
            ));
        } else {
            published.abort(path, &mut residue);
        }
        return Err(StagedError { error, residue });
    }
    // The receipt is the transaction's, never a re-read of the path.
    let owner = Owner::from_published(published).map_err(|error| StagedError {
        error,
        residue: residue.clone(),
    })?;
    Ok(Imported {
        listener,
        owner: Arc::new(SocketOwnership::from_owner(owner)),
        residue,
    })
}
