//! Network boundary for the native host. Socket access is confined to this module.

pub mod cluster_client;
pub mod frame;
pub mod http_client;
pub mod join;
pub mod listener;
pub mod replay;

pub const REGISTRY_REPLICATION_PAGE_ENTRIES: usize = 32;
pub const REGISTRY_REPLICATION_PAGE_MAX_BYTES: usize = 48 * 1024;

pub use http_client::{HttpClient, HttpRequest, HttpResponse, HttpTask};

#[cfg(any(test, feature = "http-test-support"))]
pub mod testing;
