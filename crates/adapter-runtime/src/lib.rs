//! Account-scoped native transport, protocol orchestration, and HTTP runtime.

pub mod auth;
pub mod backend;
pub mod context;
pub mod history;
mod http_routes;
pub mod images;
pub mod selection;
pub mod server;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod transport;
