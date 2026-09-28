//! Cluster vocabulary composed from identity and membership registry units.

pub mod identity;
pub mod registry;

pub use identity::{init, status, NodeIdentity};
pub use registry::{load_registry, save_registry, AuthorizedNode, NodeState, Registry};
