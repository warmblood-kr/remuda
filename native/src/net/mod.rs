//! Network boundary for the native host. Socket access is confined to this module.

pub mod http_client;

pub use http_client::{HttpClient, HttpRequest, HttpResponse, HttpTask};

#[cfg(any(test, feature = "http-test-support"))]
pub mod testing;
