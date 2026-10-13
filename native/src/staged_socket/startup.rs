//! Test-only startup transaction and bounded fault controls.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Boundary {
    Bound,
    Listening,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Failure {
    CaptureOnce,
    CapturePersistent,
    Rename,
    AfterPublish,
}

pub(crate) struct Pause {
    pub boundary: Boundary,
    pub reached: Sender<PathBuf>,
    pub release: Receiver<()>,
    pub deadline: Instant,
}

#[derive(Default)]
pub(crate) struct Controls {
    pub pause: Option<Pause>,
    pub failure: Option<Failure>,
    pub directory_mode: Option<u32>,
}

impl Controls {
    pub(crate) fn checkpoint(&self, boundary: Boundary, leaf: &Path) -> io::Result<()> {
        if let Some(pause) = &self.pause {
            if pause.boundary == boundary {
                pause
                    .reached
                    .send(leaf.to_path_buf())
                    .map_err(io::Error::other)?;
                pause
                    .release
                    .recv_timeout(pause.deadline.saturating_duration_since(Instant::now()))
                    .map_err(io::Error::other)?;
            }
        }
        Ok(())
    }
}
