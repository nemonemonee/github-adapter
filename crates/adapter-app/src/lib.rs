//! Native application integration for the migration candidate.

pub mod cli;
pub mod config;
pub mod desktop;
pub mod host;
pub mod images;
pub mod listener;
pub mod on_demand;
pub mod startup_error;
#[cfg(windows)]
mod user_security;
pub mod windows_credentials;
