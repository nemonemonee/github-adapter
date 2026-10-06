//! The stable CLI effect seam and its output/error presentation policy.

use std::ffi::OsString;
use std::net::SocketAddr;

use adapter_protocol::{AdapterError, Result, Value};
use adapter_runtime::auth::Credential;
use adapter_runtime::backend::BackendConfig;
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::{AccountScope, Provider};
use adapter_runtime::server::ServerOptions;
use futures_util::future::BoxFuture;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::options::Options;
use crate::config::{self, Client, ClientPaths, ConfigurationChange, ConfigureOptions};
use crate::desktop::DesktopApp;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Standard,
    Diagnostic,
}

/// External-effect seams. Tests provide synthetic identities/backends and never
/// call the real credential store or desktop launcher.
pub trait Services: Send + Sync {
    type Connected: Clone + Send + Sync + 'static;
    fn output(&self, channel: Output, message: &str) -> Result<()>;
    fn environment(&self, name: &str) -> Option<OsString>;
    fn paths(&self, options: &Options, clients: &[Client]) -> Result<ClientPaths>;
    fn read_credential(&self) -> BoxFuture<'_, Result<Option<String>>>;
    fn write_credential(&self, value: String) -> BoxFuture<'_, Result<()>>;
    fn delete_credential(&self) -> BoxFuture<'_, Result<bool>>;
    fn hidden_token(&self) -> BoxFuture<'_, Result<String>>;
    fn choose_provider(&self) -> BoxFuture<'_, Result<Provider>>;
    fn verify(
        &self,
        token: String,
        expected: Option<String>,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Credential>>;
    fn device_login(
        &self,
        client_id: String,
        expected: Option<String>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Credential>>;
    fn connect(
        &self,
        config: BackendConfig,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Self::Connected>>;
    fn scope(&self, backend: &Self::Connected) -> AccountScope;
    fn health(&self, backend: &Self::Connected) -> Value;
    fn redact(&self, backend: &Self::Connected, text: &str) -> String;
    fn catalog(
        &self,
        backend: Self::Connected,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Value>>;
    fn serve(
        &self,
        listener: TcpListener,
        backend: Self::Connected,
        options: ServerOptions,
        cancellation: CancellationToken,
    ) -> JoinHandle<Result<()>>;
    fn apps(&self, name: Option<String>) -> BoxFuture<'_, Result<Vec<DesktopApp>>>;
    fn launch(&self, app: DesktopApp) -> BoxFuture<'_, Result<()>>;
    fn bound(&self, _address: SocketAddr) -> Result<()> {
        Ok(())
    }
    fn configure_launch(
        &self,
        paths: ClientPaths,
        clients: Vec<Client>,
        endpoint: String,
        options: ConfigureOptions,
    ) -> BoxFuture<'_, Result<Vec<ConfigurationChange>>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                config::ensure_configured(&paths, &endpoint, &clients, &options)
            })
            .await
            .map_err(|_| {
                AdapterError::new(
                    500,
                    "configuration_task_failed",
                    "The configuration task did not complete.",
                )
            })?
        })
    }
    fn ready(&self, _address: SocketAddr) -> Result<()> {
        Ok(())
    }
}

pub(super) fn cancelled() -> AdapterError {
    AdapterError::new(499, "cancelled", "The foreground operation was cancelled.")
}

pub(super) fn safe_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            result.extend(character.escape_default());
        } else {
            result.push(character);
        }
    }
    result
}

pub fn error_exit_code(error: &AdapterError) -> u8 {
    match error.status {
        400 => 2,
        499 => 130,
        _ => 1,
    }
}

pub fn error_text(error: &AdapterError) -> String {
    format!("{}: {}", safe_text(&error.code), safe_text(&error.message))
}
