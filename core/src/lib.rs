//! remuda's policy layer — session orchestration with no operating system in it.
//!
//! # Layering, and why the compiler enforces it
//!
//! ```text
//!   remuda-core     policy / orchestration.  no host deps. builds for wasm32.
//!   ------------------------ AgentProcess ---------------------------------
//!   remuda-native   pty, processes, terminal emulation.  depends on core.
//! ```
//!
//! 정수님, 2026-09-09: *"rust also can be compiled into webasm."* wasm32 has no
//! fork, no pty and no tty, so the host layer can never target it — and this
//! crate must never depend on it. Two mechanisms hold that line, because one
//! measurement showed one of them has a gap:
//!
//! 1. **The crate boundary.** `remuda-core` lists no host dependency, so it
//!    cannot reference one. Not a rule to remember — arithmetic cargo performs.
//! 2. **`clippy.toml`.** The wasm target does *not* reject a bare `Command`,
//!    `fs`, or `Instant` written here by hand (measured: 0 errors each, because
//!    wasm32 ships std stubs that fail only at runtime). Those exact paths are
//!    denied by name, and CI runs clippy with `-D warnings`.
//!
//! ```sh
//! cargo check -p remuda-core --target wasm32-unknown-unknown
//! cargo clippy --workspace --all-targets -- -D warnings
//! ```
//!
//! # Status
//!
//! First slice. The agent seam, the injected clock and the indivisible write are
//! here because the design doc marks them as the joints that cannot be
//! retrofitted. The real pty backend lands in `remuda-native` behind
//! [`agent::AgentProcess`], so it cannot disturb what is proved here.

pub mod agent;
pub mod clock;
pub mod registry;
pub mod session;

pub use agent::{AgentError, AgentProcess, Cursor, ScriptedAgent, Size};
pub use clock::{Clock, ManualClock};
pub use registry::{Registry, SessionSummary};
pub use session::Session;
