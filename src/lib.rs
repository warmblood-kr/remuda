//! A pty manager with an embedded programming runtime — the core of the
//! cc-butler reimplementation.
//!
//! # Layering, and why a compiler enforces it
//!
//! ```text
//!   policy / orchestration      pure. no syscalls. compiles to wasm32.
//!   ---------------------- AgentProcess ------------------------------
//!   host: pty, processes, terminal emulation   native only, `native` feature
//! ```
//!
//! 정수님, 2026-09-09: *"rust also can be compiled into webasm."* wasm32 has
//! no fork, no pty and no tty, so the host layer can never target it:
//!
//! ```sh
//! cargo check --target wasm32-unknown-unknown --no-default-features
//! ```
//!
//! That gate is real but partial, and the measurement is in `Cargo.toml`:
//! it catches `std::os::unix::*` and host crates that cannot build for wasm,
//! and it does **not** catch a bare `Command`, `fs`, or `Instant` call, because
//! wasm32 ships std stubs that compile and fail only at runtime. Treat it as
//! the dependency-axis half of the boundary; the std-call axis needs a lint,
//! which wants the pure layer in its own crate.
//!
//! # Status
//!
//! First slice. The agent seam, the injected clock and the indivisible write
//! are here because the design doc marks them as the joints that cannot be
//! retrofitted. The real pty backend and the script runtime land next, both
//! behind [`agent::AgentProcess`], so neither can disturb what is proved here.

pub mod agent;
pub mod clock;
pub mod session;

pub use agent::{AgentError, AgentProcess, Cursor, ScriptedAgent, Size};
pub use clock::{Clock, ManualClock};
pub use session::Session;

#[cfg(feature = "native")]
pub use clock::SystemClock;
