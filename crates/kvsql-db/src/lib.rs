//! KvSQL DB: the `kvsqld` server and the `kvsql` shell, built on
//! KvSQL Engine. The HTTP API is described in docs/server.md.

pub mod http;
pub mod json;
pub mod server;
pub mod shell;

pub use kvsql_engine as engine;

/// Version of KvSQL DB.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
