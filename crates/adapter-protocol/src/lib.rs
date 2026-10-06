//! Transport-independent compatibility contracts for GitHub Adapter.

pub mod anthropic;
pub mod chat;
pub mod chat_stream;
pub mod compaction;
pub mod error;
pub mod identity;
pub mod json;
pub mod sse;
pub mod usage;

pub use error::{AdapterError, Result};
pub use serde_json::Value;
