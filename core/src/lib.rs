//! remuda's policy layer — session orchestration with no operating system in it.
//!
//! ```text
//!   remuda-core     policy / orchestration.  no host deps. builds for wasm32.
//!   ------------------------ AgentProcess ---------------------------------
//!   remuda-native   pty, processes, terminal emulation.  depends on core.
//! ```
//!
//! Two mechanisms hold that line, not one: the **crate boundary** (no host
//! dependency is listed, so none can be named — arithmetic cargo performs) and
//! **`clippy.toml`**, which denies `Command`, `fs` and `Instant` by name.
//!
//! ⚠ Neither is redundant. The wasm target does *not* reject those paths on its
//! own — it ships std stubs that compile and fail only at runtime — so dropping
//! `clippy.toml` silently reopens the axis (`PRINCIPLES.md` §1).
//!
//! ```sh
//! cargo check -p remuda-core --target wasm32-unknown-unknown
//! cargo clippy --workspace --all-targets -- -D warnings
//! ```

pub mod agent;
pub mod clock;
pub mod keys;
pub mod protocol;
pub mod registry;
pub mod session;

pub use agent::{AgentError, AgentProcess, Cursor, ScriptedAgent, Size};
pub use clock::{Clock, ManualClock};
pub use registry::{Registry, SessionSummary};
pub use session::Session;
