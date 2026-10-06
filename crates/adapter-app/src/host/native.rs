use std::ffi::OsString;
use std::net::SocketAddr;
use std::time::Duration;

use adapter_protocol::{Result, Value};
use adapter_runtime::auth::Credential;
use adapter_runtime::backend::BackendConfig;
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::{AccountScope, Provider};
use adapter_runtime::server::ServerOptions;
use futures_util::future::BoxFuture;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cli::{self, NativeServices, Options, Output, Services};
use crate::config::{Client, ClientPaths, ConfigurationChange, ConfigureOptions};
use crate::desktop::{self, DesktopApp};
use crate::windows_credentials;
use std::sync::Arc;

use super::{Driver, ENVIRONMENT, HostHandle, Inputs, failure};

async fn saved_credential() -> Result<Option<String>> {
    tokio::time::timeout(
        Duration::from_secs(3),
        tokio::task::spawn_blocking(|| {
            windows_credentials::read(cli::CREDENTIAL_SERVICE, cli::CREDENTIAL_USER)
        }),
    )
    .await
    .map_err(|_| {
        failure(
            "credential_unavailable",
            "The current-user credential store did not respond in time.",
        )
    })?
    .map_err(|_| {
        failure(
            "credential_unavailable",
            "The current-user credential operation failed.",
        )
    })?
}

pub(super) async fn capture_inputs() -> Result<Inputs> {
    Ok(Inputs::new(
        ENVIRONMENT
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| ((*name).into(), value))),
        saved_credential().await?,
    ))
}

pub(super) struct NativeDriver {
    pub(super) options: Options,
    pub(super) inputs: Inputs,
    pub(super) mode: Arc<crate::on_demand::Session>,
}

impl Driver for NativeDriver {
    fn serve(
        &self,
        host: HostHandle,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<i32>> {
        Box::pin(async move {
            if self.options.setup && self.options.clients.selected().contains(&Client::Codex) {
                let native = NativeServices::default();
                let paths = native.paths(&self.options, &[Client::Codex])?;
                let port = self.options.address.port();
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = crate::images::startup(&paths) {
                        eprintln!(
                            "Optional image registration needs attention: {}",
                            cli::error_text(&error)
                        );
                    }
                    crate::on_demand::restore_normal_if_stopped(paths, port)
                })
                .await
                .map_err(|_| {
                    failure(
                        "normal_recovery_failed",
                        "Previous normal routing recovery did not complete.",
                    )
                })??;
            }
            let observer = host.clone();
            let services = BackgroundServices {
                native: NativeServices::with_ready_observer(move |address| {
                    observer.backend_ready(address)
                }),
                inputs: self.inputs.clone(),
                host,
                mode: self.mode.clone(),
            };
            let request = cli::dispatch(&services, self.options.clone(), cancellation.clone());
            tokio::pin!(request);
            let mut companion_failed = false;
            let result = loop {
                tokio::select! {
                    result = &mut request => break result,
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        if self.mode.companion_failed() {
                            companion_failed = true;
                            cancellation.cancel();
                        }
                    }
                }
            };
            let mode = self.mode.clone();
            tokio::task::spawn_blocking(move || mode.finish())
                .await
                .map_err(|_| {
                    failure(
                        "normal_restore_failed",
                        "Normal Codex restoration did not complete.",
                    )
                })??;
            if companion_failed {
                return Err(failure(
                    "recovery_companion_failed",
                    "The recovery companion exited; normal routing was restored and the adapter stopped.",
                ));
            }
            result
        })
    }

    fn open_codex(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let current = Inputs {
                environment: self.inputs.environment.clone(),
                saved_credential: saved_credential().await?,
            };
            if current.credential_stamp() != self.inputs.credential_stamp() {
                return Err(failure(
                    "restart_required",
                    "The saved credential changed. Stop/start before activating Codex.",
                ));
            }
            tokio::task::spawn_blocking(|| {
                let app = desktop::select("codex", &desktop::discover()?)?;
                desktop::launch(&app)
            })
            .await
            .map_err(|_| {
                failure(
                    "activation_timeout",
                    "Activation ended without an acknowledgment.",
                )
            })?
        })
    }
}

struct BackgroundServices {
    native: NativeServices,
    inputs: Inputs,
    host: HostHandle,
    mode: Arc<crate::on_demand::Session>,
}

fn interactive_disabled<T>() -> BoxFuture<'static, Result<T>> {
    Box::pin(async {
        Err(failure(
            "background_interactive_disabled",
            "Account writes and interactive prompts are disabled in the background host.",
        ))
    })
}

fn claude_configuration(mut options: ConfigureOptions, codex_selected: bool) -> ConfigureOptions {
    if codex_selected {
        // These overrides were consumed by the separate on-demand Codex setup.
        // Keep Claude-only requests unchanged so invalid options still fail validation.
        options.force_openai_provider = false;
        options.model = None;
        options.reasoning_effort = None;
    }
    options
}

impl Services for BackgroundServices {
    type Connected = cli::ConnectedBackend;

    fn configure_launch(
        &self,
        paths: ClientPaths,
        clients: Vec<Client>,
        endpoint: String,
        options: ConfigureOptions,
    ) -> BoxFuture<'_, Result<Vec<ConfigurationChange>>> {
        Box::pin(async move {
            let mut changes = Vec::new();
            if clients.contains(&Client::Codex) {
                let mode = self.mode.clone();
                let selected_paths = paths.clone();
                let selected_endpoint = endpoint.clone();
                let selected_options = options.clone();
                changes.extend(
                    tokio::task::spawn_blocking(move || {
                        mode.configure(selected_paths, selected_endpoint, selected_options)
                    })
                    .await
                    .map_err(|_| {
                        failure(
                            "configuration_task_failed",
                            "On-demand configuration did not complete.",
                        )
                    })??,
                );
            }
            if clients.contains(&Client::Claude) {
                changes.extend(
                    self.native
                        .configure_launch(
                            paths,
                            vec![Client::Claude],
                            endpoint,
                            claude_configuration(options, clients.contains(&Client::Codex)),
                        )
                        .await?,
                );
            }
            Ok(changes)
        })
    }
    fn output(&self, _channel: Output, _message: &str) -> Result<()> {
        // CLI output may name settings or upstreams. The host exposes only fixed, bounded events.
        Ok(())
    }
    fn environment(&self, name: &str) -> Option<OsString> {
        self.inputs.environment.get(name).cloned()
    }
    fn paths(&self, options: &Options, clients: &[Client]) -> Result<ClientPaths> {
        self.native.paths(options, clients)
    }
    fn read_credential(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async { Ok(self.inputs.saved_credential.clone()) })
    }
    fn write_credential(&self, _value: String) -> BoxFuture<'_, Result<()>> {
        interactive_disabled()
    }
    fn delete_credential(&self) -> BoxFuture<'_, Result<bool>> {
        interactive_disabled()
    }
    fn hidden_token(&self) -> BoxFuture<'_, Result<String>> {
        interactive_disabled()
    }
    fn choose_provider(&self) -> BoxFuture<'_, Result<Provider>> {
        interactive_disabled()
    }
    fn verify(
        &self,
        token: String,
        expected: Option<String>,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Credential>> {
        self.native.verify(token, expected, context)
    }
    fn device_login(
        &self,
        _client_id: String,
        _expected: Option<String>,
        _cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Credential>> {
        interactive_disabled()
    }
    fn connect(
        &self,
        config: BackendConfig,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Self::Connected>> {
        self.native.connect(config, context)
    }
    fn scope(&self, backend: &Self::Connected) -> AccountScope {
        self.native.scope(backend)
    }
    fn health(&self, backend: &Self::Connected) -> Value {
        self.native.health(backend)
    }
    fn redact(&self, backend: &Self::Connected, text: &str) -> String {
        self.native.redact(backend, text)
    }
    fn catalog(
        &self,
        backend: Self::Connected,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Value>> {
        self.native.catalog(backend, context)
    }
    fn serve(
        &self,
        listener: TcpListener,
        backend: Self::Connected,
        options: ServerOptions,
        cancellation: CancellationToken,
    ) -> JoinHandle<Result<()>> {
        self.native.serve(listener, backend, options, cancellation)
    }
    fn apps(&self, name: Option<String>) -> BoxFuture<'_, Result<Vec<DesktopApp>>> {
        self.native.apps(name)
    }
    fn launch(&self, _app: DesktopApp) -> BoxFuture<'_, Result<()>> {
        // API readiness is not an activation acknowledgment. The separately owned activation
        // worker reports its outcome, and an activation failure does not discard a ready server.
        Box::pin(async { self.host.initial_open() })
    }
    fn ready(&self, address: SocketAddr) -> Result<()> {
        self.native.ready(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_client_setup_scopes_codex_overrides_without_weakening_claude_validation() {
        let options = ConfigureOptions {
            dry_run: true,
            force_openai_provider: true,
            model: Some("selected-codex-model".into()),
            reasoning_effort: Some("max".into()),
        };
        let claude = claude_configuration(options.clone(), true);
        assert!(claude.dry_run);
        assert!(!claude.force_openai_provider);
        assert!(claude.model.is_none());
        assert!(claude.reasoning_effort.is_none());
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("claude.json");
        let original = b"{\"model\":\"preserve-claude-model\"}\n";
        std::fs::write(&target, original).unwrap();
        let paths = ClientPaths {
            codex_config: Some(root.path().join("unused-codex.toml")),
            claude_settings: Some(target.clone()),
            backup_dir: root.path().join("backup"),
        };
        let endpoint = "http://127.0.0.1:54321";
        assert_eq!(
            crate::config::configure(&paths, endpoint, &[Client::Claude], &claude)
                .unwrap()
                .len(),
            1
        );
        let unscoped = claude_configuration(options, false);
        assert!(crate::config::configure(&paths, endpoint, &[Client::Claude], &unscoped).is_err());
        assert_eq!(std::fs::read(target).unwrap(), original);
        assert!(!paths.codex_config.unwrap().exists());
    }
}
