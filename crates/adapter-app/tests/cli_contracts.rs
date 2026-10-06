#![cfg(any(windows, target_os = "macos"))]

use std::collections::HashMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adapter_app::cli::{
    self, CLIENT_ID_ENV, CREDENTIAL_SERVICE, CREDENTIAL_USER, Clients, Command, DEFAULT_CLIENT_ID,
    DEFAULT_PROBE_TIMEOUT, LEGACY_CLIENT_ID_ENV, LEGACY_TOKEN_ENV, Launch, NativeServices, Options,
    Output, Parsed, ProviderMode, Services, TOKEN_ENV,
};
use adapter_app::config::ClientPaths;
use adapter_app::desktop::DesktopApp;
use adapter_app::listener;
use adapter_protocol::{AdapterError, Result, Value};
use adapter_runtime::auth::Credential;
use adapter_runtime::backend::BackendConfig;
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::{AccountScope, Provider};
use adapter_runtime::server::ServerOptions;
use futures_util::future::BoxFuture;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Barrier, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-oauth-token";

fn credential(token: &str) -> Credential {
    Credential::new("alice".into(), token.into()).unwrap()
}

fn parsed(arguments: &[&str]) -> Options {
    match cli::parse_from(std::iter::once("adapter").chain(arguments.iter().copied())).unwrap() {
        Parsed::Run(options) => *options,
        Parsed::Display(_) => panic!("expected runnable options"),
    }
}

fn model(id: &str) -> Value {
    json!({
        "slug": id, "context_window": 4096,
        "default_reasoning_level": "high",
        "supported_reasoning_levels": [{"effort":"high"}, {"effort":"max"}]
    })
}

fn catalog(scope: &AccountScope, ids: &[String]) -> Value {
    json!({
        "provider": if scope.provider == Provider::Mai { "mai" } else { "github" },
        "account": if scope.provider == Provider::Github { json!(scope.account) } else { Value::Null },
        "models": ids.iter().map(|id| model(id)).collect::<Vec<_>>(),
        "data": ids.iter().map(|id| json!({"id":id,"object":"model"})).collect::<Vec<_>>(),
        "object": "list"
    })
}

#[derive(Clone)]
struct FakeBackend {
    scope: AccountScope,
    catalog: Value,
    health: Value,
    token: Option<String>,
}

struct State {
    env: HashMap<String, OsString>,
    stored: Option<String>,
    events: Vec<String>,
    output: Vec<(Output, String)>,
    seen_tokens: Vec<String>,
    seen_client_ids: Vec<String>,
    mai_models: Vec<String>,
    github_models: Vec<String>,
    verify_error: bool,
    connect_error: bool,
    hidden_error: bool,
    bad_health: bool,
    missing_local_model: bool,
    choice: Provider,
    barrier: Option<Arc<Barrier>>,
    connect_delay: Duration,
    catalog_delay: Duration,
    cancel_on_launch: Option<CancellationToken>,
    cancel_on_ready: Option<CancellationToken>,
    pending_launch: bool,
    bound_address: Option<SocketAddr>,
    ready_address: Option<SocketAddr>,
}

#[derive(Clone)]
struct FakeServices {
    state: Arc<Mutex<State>>,
    paths: ClientPaths,
}

struct Fixture {
    root: tempfile::TempDir,
    services: FakeServices,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("native-cli-contract-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let paths = ClientPaths {
            codex_config: Some(root.path().join("Codex space").join("config.toml")),
            claude_settings: Some(root.path().join("Claude space").join("settings.json")),
            backup_dir: root.path().join("backups"),
        };
        let state = State {
            env: HashMap::from([(
                "CODEX_HOME".into(),
                root.path().join("Codex cache").into_os_string(),
            )]),
            stored: Some(cli::encode_credential(&credential(TOKEN)).unwrap()),
            events: Vec::new(),
            output: Vec::new(),
            seen_tokens: Vec::new(),
            seen_client_ids: Vec::new(),
            mai_models: vec!["gpt-6-astra".into(), "gpt-5.6-sol".into()],
            github_models: vec!["gpt-6-astra".into(), "gpt-5.6-sol".into()],
            verify_error: false,
            connect_error: false,
            hidden_error: false,
            bad_health: false,
            missing_local_model: false,
            choice: Provider::Mai,
            barrier: None,
            connect_delay: Duration::ZERO,
            catalog_delay: Duration::ZERO,
            cancel_on_launch: None,
            cancel_on_ready: None,
            pending_launch: false,
            bound_address: None,
            ready_address: None,
        };
        Self {
            root,
            services: FakeServices {
                state: Arc::new(Mutex::new(state)),
                paths,
            },
        }
    }

    fn write_codex(&self, bytes: &[u8]) {
        let path = self.services.paths.codex_config.as_ref().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn codex(&self) -> Vec<u8> {
        std::fs::read(self.services.paths.codex_config.as_ref().unwrap()).unwrap()
    }

    fn events(&self) -> Vec<String> {
        self.services.state.lock().unwrap().events.clone()
    }

    fn output(&self) -> String {
        self.services
            .state
            .lock()
            .unwrap()
            .output
            .iter()
            .map(|(_, text)| text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn run(&self, args: &[&str]) -> Result<i32> {
        cli::run_with(
            &self.services,
            std::iter::once("adapter").chain(args.iter().copied()),
            CancellationToken::new(),
        )
        .await
    }
}

impl Services for FakeServices {
    type Connected = FakeBackend;

    fn output(&self, channel: Output, message: &str) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .output
            .push((channel, message.into()));
        Ok(())
    }

    fn environment(&self, name: &str) -> Option<OsString> {
        let mut state = self.state.lock().unwrap();
        state.events.push(format!("env:{name}"));
        state.env.get(name).cloned()
    }

    fn paths(
        &self,
        options: &Options,
        clients: &[adapter_app::config::Client],
    ) -> Result<ClientPaths> {
        self.state.lock().unwrap().events.push("paths".into());
        let selected = |client| clients.contains(&client);
        Ok(ClientPaths {
            codex_config: options.codex_config.clone().or_else(|| {
                selected(adapter_app::config::Client::Codex)
                    .then(|| self.paths.codex_config.clone())
                    .flatten()
            }),
            claude_settings: options.claude_settings.clone().or_else(|| {
                selected(adapter_app::config::Client::Claude)
                    .then(|| self.paths.claude_settings.clone())
                    .flatten()
            }),
            backup_dir: options
                .backup_dir
                .clone()
                .unwrap_or_else(|| self.paths.backup_dir.clone()),
        })
    }

    fn read_credential(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async {
            let mut state = self.state.lock().unwrap();
            state.events.push("store:read".into());
            Ok(state.stored.clone())
        })
    }

    fn write_credential(&self, value: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.events.push("store:write".into());
            state.stored = Some(value);
            Ok(())
        })
    }

    fn delete_credential(&self) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async {
            let mut state = self.state.lock().unwrap();
            state.events.push("store:delete".into());
            Ok(state.stored.take().is_some())
        })
    }

    fn hidden_token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async {
            let mut state = self.state.lock().unwrap();
            state.events.push("hidden-token".into());
            if state.hidden_error {
                Err(AdapterError::invalid(
                    "Hidden console input is unavailable.",
                ))
            } else {
                Ok(TOKEN.into())
            }
        })
    }

    fn choose_provider(&self) -> BoxFuture<'_, Result<Provider>> {
        Box::pin(async {
            let mut state = self.state.lock().unwrap();
            state.events.push("choose-provider".into());
            Ok(state.choice)
        })
    }

    fn verify(
        &self,
        token: String,
        expected: Option<String>,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Credential>> {
        Box::pin(async move {
            context.check()?;
            let mut state = self.state.lock().unwrap();
            state.events.push("verify".into());
            state.seen_tokens.push(token.clone());
            if state.verify_error {
                return Err(AdapterError::new(
                    401,
                    "fixture_verify",
                    format!("Rejected {token}"),
                ));
            }
            let credential = Credential::new("alice".into(), token)?;
            if expected
                .as_ref()
                .is_some_and(|login| !login.eq_ignore_ascii_case("alice"))
            {
                return Err(AdapterError::new(401, "fixture_account", "Wrong account."));
            }
            Ok(credential)
        })
    }

    fn device_login(
        &self,
        client_id: String,
        _expected: Option<String>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Credential>> {
        Box::pin(async move {
            assert!(!cancellation.is_cancelled());
            {
                let mut state = self.state.lock().unwrap();
                state.events.push("device-login".into());
                state.seen_client_ids.push(client_id);
            }
            self.output(
                Output::Standard,
                "Open https://github.com/login/device\nEnter code: TEST-CODE",
            )?;
            Ok(credential(TOKEN))
        })
    }

    fn connect(
        &self,
        config: BackendConfig,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Self::Connected>> {
        Box::pin(async move {
            let _guard = context.cancel_on_drop();
            let (scope, token) = match config {
                BackendConfig::Mai { upstream, .. } => {
                    (AccountScope::new(Provider::Mai, upstream)?, None)
                }
                BackendConfig::Github { credential } => (
                    AccountScope::new(Provider::Github, credential.login.clone())?,
                    Some(credential.token().to_owned()),
                ),
            };
            let (delay, barrier, fail, ids, bad_health) = {
                let mut state = self.state.lock().unwrap();
                state.events.push(format!(
                    "connect:{}",
                    if scope.provider == Provider::Mai {
                        "mai"
                    } else {
                        "github"
                    }
                ));
                (
                    state.connect_delay,
                    state.barrier.clone(),
                    state.connect_error,
                    if scope.provider == Provider::Mai {
                        state.mai_models.clone()
                    } else {
                        state.github_models.clone()
                    },
                    state.bad_health,
                )
            };
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
            tokio::time::sleep(delay).await;
            context.check()?;
            if fail {
                return Err(AdapterError::upstream(format!(
                    "Readiness rejected {}",
                    token.as_deref().unwrap_or("MAI")
                )));
            }
            let catalog = catalog(&scope, &ids);
            let mut health = json!({
                "application":"github-adapter", "engine":"rust", "status":"ok",
                "provider":if scope.provider == Provider::Mai { "mai" } else { "github" },
                "account":if scope.provider == Provider::Github { json!(scope.account) } else { Value::Null },
                "upstream":if scope.provider == Provider::Mai { scope.account.as_str() } else { "https://api.githubcopilot.com" }
            });
            if bad_health {
                health["engine"] = json!("different-process");
            }
            Ok(FakeBackend {
                scope,
                catalog,
                health,
                token,
            })
        })
    }

    fn scope(&self, backend: &Self::Connected) -> AccountScope {
        backend.scope.clone()
    }
    fn health(&self, backend: &Self::Connected) -> Value {
        backend.health.clone()
    }
    fn redact(&self, backend: &Self::Connected, text: &str) -> String {
        backend.token.as_ref().map_or_else(
            || text.to_owned(),
            |token| text.replace(token, "[redacted]"),
        )
    }

    fn catalog(
        &self,
        backend: Self::Connected,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            let delay = {
                let mut state = self.state.lock().unwrap();
                state.events.push("catalog".into());
                state.catalog_delay
            };
            tokio::time::sleep(delay).await;
            context.check()?;
            Ok(backend.catalog)
        })
    }

    fn serve(
        &self,
        listener: TcpListener,
        backend: Self::Connected,
        _options: ServerOptions,
        cancellation: CancellationToken,
    ) -> JoinHandle<Result<()>> {
        self.state.lock().unwrap().events.push("serve".into());
        let state = self.state.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let (mut stream, _) =
                    accepted.map_err(|_| AdapterError::upstream("Fixture accept failed."))?;
                let bytes = tokio::select! {
                    _ = cancellation.cancelled() => break,
                    bytes = request(&mut stream) => bytes?,
                };
                let path = String::from_utf8_lossy(&bytes)
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                let value = {
                    let mut state = state.lock().unwrap();
                    state.events.push(format!("http:{path}"));
                    if path == "/health" {
                        backend.health.clone()
                    } else if state.missing_local_model {
                        let mut catalog = backend.catalog.clone();
                        catalog["models"] = json!([]);
                        catalog
                    } else {
                        backend.catalog.clone()
                    }
                };
                reply(
                    &mut stream,
                    "application/json",
                    &serde_json::to_vec(&value).unwrap(),
                )
                .await?;
            }
            state.lock().unwrap().events.push("server-stopped".into());
            Ok(())
        })
    }

    fn apps(&self, name: Option<String>) -> BoxFuture<'_, Result<Vec<DesktopApp>>> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(format!("apps:{}", name.as_deref().unwrap_or("all")));
            Ok(vec![DesktopApp {
                name: "Codex".into(),
                target: "synthetic-app-target".into(),
            }])
        })
    }

    fn launch(&self, _app: DesktopApp) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            let pending = {
                let mut state = self.state.lock().unwrap();
                state.events.push("launch".into());
                if let Some(cancel) = &state.cancel_on_launch {
                    cancel.cancel();
                }
                state.pending_launch
            };
            if pending {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn bound(&self, address: SocketAddr) -> Result<()> {
        assert!(
            listener::bind_loopback(address).is_err(),
            "the listener must stay owned during preflight"
        );
        let mut state = self.state.lock().unwrap();
        state.events.push("bound".into());
        state.bound_address = Some(address);
        Ok(())
    }

    fn ready(&self, address: SocketAddr) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.events.push("ready".into());
        state.ready_address = Some(address);
        if let Some(cancel) = &state.cancel_on_ready {
            cancel.cancel();
        }
        Ok(())
    }
}

async fn request(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = stream
            .read(&mut buffer)
            .await
            .map_err(|_| AdapterError::upstream("Fixture read failed."))?;
        if count == 0 {
            return Err(AdapterError::upstream("Fixture request ended early."));
        }
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 64 * 1024);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return Ok(bytes);
            }
        }
    }
}

async fn reply(stream: &mut TcpStream, content_type: &str, body: &[u8]) -> Result<()> {
    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes())
        .await.map_err(|_| AdapterError::upstream("Fixture write failed."))?;
    stream
        .write_all(body)
        .await
        .map_err(|_| AdapterError::upstream("Fixture write failed."))?;
    stream
        .shutdown()
        .await
        .map_err(|_| AdapterError::upstream("Fixture shutdown failed."))?;
    Ok(())
}

#[test]
fn no_arguments_choose_auto_but_explicit_selectors_remain_manual() {
    let options = parsed(&[]);
    assert_eq!(options.command, Command::Serve);
    assert_eq!(options.provider, ProviderMode::Auto);
    assert!(options.setup);
    assert_eq!(options.launch, Some(Launch::Codex));
    assert_eq!(options.clients, Clients::Codex);
    assert_eq!(options.probe_timeout, DEFAULT_PROBE_TIMEOUT);
    assert_eq!(options.preferred_models, ["gpt-6-astra", "gpt-5.6-sol"]);
    for (args, provider) in [
        (vec!["serve"], ProviderMode::Mai),
        (vec!["mai"], ProviderMode::Mai),
        (vec!["github"], ProviderMode::Github),
        (vec!["--provider", "github"], ProviderMode::Github),
        (vec!["serve", "--provider", "auto"], ProviderMode::Auto),
    ] {
        let options = parsed(&args);
        assert_eq!(options.command, Command::Serve);
        assert_eq!(options.provider, provider);
        assert!(!options.setup);
        assert_eq!(options.launch, None);
    }
}

#[test]
fn invalid_combinations_are_rejected_without_echoing_argument_values() {
    for args in [
        vec!["auto", "--provider", "github"],
        vec!["auto", "--choose-provider"],
        vec!["github", "--preferred-model", "one"],
        vec!["mai", "--probe-timeout", "2"],
        vec!["auto", "--client", "claude"],
        vec!["auto", "--dry-run"],
        vec!["auto", "--probe-timeout", "nan"],
        vec!["auto", "--probe-timeout", "0"],
        vec![
            "auto",
            "--preferred-model",
            "same",
            "--preferred-model",
            "same",
        ],
        vec!["setup", "--provider", "github"],
        vec!["doctor", "--setup"],
        vec!["account", "--token"],
        vec!["models", "--launch", "codex"],
        vec!["apps", "--codex-config", "ignored"],
        vec!["serve", "--force-openai-provider"],
        vec!["github", "--upstream", "https://example.invalid"],
        vec!["serve", "--host", "0.0.0.0"],
        vec!["serve", "--host", "example.invalid"],
        vec!["setup", "--port", "0"],
        vec!["doctor", "--port", "0"],
    ] {
        let error = cli::parse_from(std::iter::once("adapter").chain(args)).unwrap_err();
        assert_eq!(error.status, 400);
    }
    let error = cli::parse_from(["adapter", "login", "--token", TOKEN]).unwrap_err();
    assert!(!cli::error_text(&error).contains(TOKEN));
    assert_eq!(cli::error_exit_code(&error), 2);
    assert_eq!(
        cli::error_exit_code(&AdapterError::new(499, "cancelled", "cancelled")),
        130
    );
}

#[test]
fn loopback_ipv6_ephemeral_ports_and_explicit_priority_are_supported() {
    for host in ["::1", "[::1]", "localhost", "127.0.0.2"] {
        let options = parsed(&["serve", "--host", host, "--port", "0"]);
        assert!(options.address.ip().is_loopback());
        assert_eq!(options.address.port(), 0);
    }
    assert_eq!(
        cli::endpoint("[::1]:1234".parse().unwrap()),
        "http://[::1]:1234"
    );
    let options = parsed(&[
        "models",
        "--provider",
        "auto",
        "--preferred-model",
        "second",
        "--preferred-model",
        "first",
        "--probe-timeout",
        "0.2",
    ]);
    assert_eq!(options.preferred_models, ["second", "first"]);
    assert_eq!(options.probe_timeout, Duration::from_millis(200));
}

#[test]
fn reasoning_uses_only_the_advertised_menu_and_keeps_compatible_overrides() {
    assert_eq!(
        cli::compatible_reasoning_effort(&model("fixture"), Some("ultra")).unwrap(),
        Some("max".into())
    );
    assert_eq!(
        cli::compatible_reasoning_effort(&model("fixture"), Some("high")).unwrap(),
        Some("high".into())
    );
    assert_eq!(
        cli::compatible_reasoning_effort(&model("fixture"), Some("low")).unwrap(),
        Some("high".into())
    );
    assert_eq!(
        cli::compatible_reasoning_effort(&json!({}), None).unwrap(),
        None
    );
    assert!(cli::compatible_reasoning_effort(&json!({}), Some("ultra")).is_err());
    assert!(
        cli::compatible_reasoning_effort(
            &json!({"supported_reasoning_levels":[{"effort":"high"}]}),
            Some("low")
        )
        .is_err()
    );
}

#[tokio::test]
async fn help_version_and_apps_do_not_resolve_environments_credentials_or_providers() {
    for args in [vec!["--help"], vec!["--version"], vec!["apps"]] {
        let fixture = Fixture::new();
        assert_eq!(fixture.run(&args).await.unwrap(), 0);
        assert!(fixture.events().iter().all(|event| event == "apps:all"));
        assert_eq!(std::fs::read_dir(fixture.root.path()).unwrap().count(), 0);
    }
}

#[test]
fn credential_namespace_and_v1_serialization_remain_python_compatible() {
    assert_eq!(CREDENTIAL_SERVICE, "MAI Adapter");
    assert_eq!(CREDENTIAL_USER, "personal-github");
    let encoded = cli::encode_credential(&credential(TOKEN)).unwrap();
    assert_eq!(
        encoded,
        format!("{{\"version\":1,\"login\":\"alice\",\"token\":\"{TOKEN}\"}}")
    );
    assert_eq!(cli::decode_credential(&encoded).unwrap().token(), TOKEN);
    for value in [
        "invalid",
        r#"{"version":true,"login":"alice","token":"synthetic"}"#,
        r#"{"version":2,"login":"alice","token":"synthetic"}"#,
        r#"{"version":1,"login":"","token":"synthetic"}"#,
        r#"{"version":1,"login":"alice","token":null}"#,
    ] {
        let error = cli::decode_credential(value).unwrap_err();
        assert_eq!(error.status, 401);
        assert!(!error.message.contains("synthetic"));
    }
}

#[tokio::test]
async fn saved_account_is_offline_and_ambient_tokens_are_never_adopted() {
    let fixture = Fixture::new();
    {
        let mut state = fixture.services.state.lock().unwrap();
        state.env.insert("GH_TOKEN".into(), "ambient-one".into());
        state
            .env
            .insert("GITHUB_TOKEN".into(), "ambient-two".into());
    }
    assert_eq!(
        fixture
            .run(&["account", "--github-account", "ALICE"])
            .await
            .unwrap(),
        0
    );
    assert!(
        fixture
            .output()
            .contains("Saved personal GitHub account: alice")
    );
    assert_eq!(
        fixture.events(),
        [
            "env:GITHUB_ADAPTER_TOKEN",
            "env:MAI_ADAPTER_GITHUB_TOKEN",
            "store:read"
        ]
    );
    fixture.services.state.lock().unwrap().stored = None;
    assert_eq!(fixture.run(&["account"]).await.unwrap(), 1);
    assert!(
        fixture
            .services
            .state
            .lock()
            .unwrap()
            .seen_tokens
            .is_empty()
    );
}

#[tokio::test]
async fn explicit_adapter_tokens_are_verified_never_saved_and_fail_without_fallback() {
    for (token, fail) in [
        ("explicit-synthetic-token", false),
        ("", false),
        ("explicit-synthetic-token", true),
    ] {
        let fixture = Fixture::new();
        let original = {
            let mut state = fixture.services.state.lock().unwrap();
            state.env.insert(TOKEN_ENV.into(), token.into());
            state.verify_error = fail;
            state.stored.clone()
        };
        let result = fixture.run(&["account"]).await;
        if fail || token.is_empty() {
            let error = result.unwrap_err();
            assert_eq!(error.status, 401);
            if !token.is_empty() {
                assert!(!error.message.contains(token));
            }
        } else {
            assert_eq!(result.unwrap(), 0);
            assert!(fixture.output().contains(TOKEN_ENV));
            assert!(!fixture.output().contains(token));
        }
        let state = fixture.services.state.lock().unwrap();
        assert_eq!(state.stored, original);
        assert!(
            !state
                .events
                .iter()
                .any(|event| event.starts_with("store:") || event.starts_with("connect:"))
        );
    }
}

#[tokio::test]
async fn adapter_token_aliases_agree_or_fail_before_any_account_is_read() {
    for equal in [false, true] {
        let fixture = Fixture::new();
        {
            let mut state = fixture.services.state.lock().unwrap();
            state.env.insert(TOKEN_ENV.into(), "first-synthetic".into());
            state.env.insert(
                LEGACY_TOKEN_ENV.into(),
                if equal {
                    "first-synthetic"
                } else {
                    "second-synthetic"
                }
                .into(),
            );
        }
        let result = fixture.run(&["account"]).await;
        if equal {
            assert_eq!(result.unwrap(), 0);
        } else {
            assert_eq!(result.unwrap_err().code, "credential_conflict");
        }
        assert!(!fixture.events().contains(&"store:read".into()));
    }
}

#[tokio::test]
async fn login_saves_only_after_identity_and_backend_readiness() {
    for token_login in [false, true] {
        for fail in [false, true] {
            let fixture = Fixture::new();
            let original = {
                let mut state = fixture.services.state.lock().unwrap();
                state.connect_error = fail;
                state.stored = Some("previous-synthetic-credential".into());
                state.stored.clone()
            };
            let args = if token_login {
                vec!["login", "--token"]
            } else {
                vec!["login"]
            };
            let result = fixture.run(&args).await;
            let state = fixture.services.state.lock().unwrap();
            if fail {
                let error = result.unwrap_err();
                assert!(!error.message.contains(TOKEN));
                assert_eq!(state.stored, original);
                assert!(!state.events.contains(&"store:write".into()));
            } else {
                assert_eq!(result.unwrap(), 0);
                assert_eq!(
                    cli::decode_credential(state.stored.as_deref().unwrap())
                        .unwrap()
                        .login,
                    "alice"
                );
                assert!(
                    state
                        .events
                        .iter()
                        .position(|event| event == "connect:github")
                        .unwrap()
                        < state
                            .events
                            .iter()
                            .position(|event| event == "store:write")
                            .unwrap()
                );
                if !token_login {
                    assert_eq!(state.seen_client_ids, [DEFAULT_CLIENT_ID]);
                    assert!(
                        state
                            .output
                            .iter()
                            .any(|(_, text)| text.contains("TEST-CODE"))
                    );
                }
            }
            assert!(state.output.iter().all(|(_, text)| !text.contains(TOKEN)));
        }
    }
}

#[tokio::test]
async fn client_id_alias_conflicts_are_login_only_and_explicit_flags_win() {
    let fixture = Fixture::new();
    {
        let mut state = fixture.services.state.lock().unwrap();
        state
            .env
            .insert(CLIENT_ID_ENV.into(), "first-public-client".into());
        state
            .env
            .insert(LEGACY_CLIENT_ID_ENV.into(), "second-public-client".into());
    }
    assert_eq!(
        fixture.run(&["login"]).await.unwrap_err().code,
        "credential_conflict"
    );
    assert!(!fixture.events().contains(&"device-login".into()));
    assert_eq!(
        fixture
            .run(&["login", "--github-client-id", "explicit-public-client"])
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        fixture.services.state.lock().unwrap().seen_client_ids,
        ["explicit-public-client"]
    );
}

#[tokio::test]
async fn hidden_input_failure_never_uses_an_echoing_or_saved_token_fallback() {
    let fixture = Fixture::new();
    fixture.services.state.lock().unwrap().hidden_error = true;
    assert_eq!(
        fixture.run(&["login", "--token"]).await.unwrap_err().status,
        400
    );
    let events = fixture.events();
    assert!(events.contains(&"hidden-token".into()));
    assert!(!events.iter().any(|event| event == "verify"
        || event.starts_with("store:")
        || event.starts_with("connect:")));
}

#[tokio::test]
async fn logout_removes_only_the_saved_entry_and_does_not_revoke_running_auth() {
    let fixture = Fixture::new();
    fixture
        .services
        .state
        .lock()
        .unwrap()
        .env
        .insert(TOKEN_ENV.into(), TOKEN.into());
    assert_eq!(fixture.run(&["logout"]).await.unwrap(), 0);
    assert!(fixture.services.state.lock().unwrap().stored.is_none());
    assert_eq!(
        fixture.events(),
        [
            "store:delete",
            "env:GITHUB_ADAPTER_TOKEN",
            "env:MAI_ADAPTER_GITHUB_TOKEN"
        ]
    );
    assert!(fixture.output().contains("not revoked"));
    assert!(fixture.output().contains("No running process was stopped"));
    assert!(!fixture.output().contains(TOKEN));
}

#[tokio::test]
async fn automatic_discovery_is_concurrent_and_obeys_model_priority_then_mai_ties() {
    for github_better in [false, true] {
        let fixture = Fixture::new();
        {
            let mut state = fixture.services.state.lock().unwrap();
            state.barrier = Some(Arc::new(Barrier::new(2)));
            if github_better {
                state.mai_models = vec!["gpt-5.6-sol".into()];
            }
        }
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                fixture.run(&["models", "--provider", "auto"])
            )
            .await
            .unwrap()
            .unwrap(),
            0
        );
        let state = fixture.services.state.lock().unwrap();
        let output = state
            .output
            .iter()
            .find(|(channel, _)| *channel == Output::Standard)
            .unwrap();
        let catalog: Value = serde_json::from_str(&output.1).unwrap();
        assert_eq!(
            catalog["provider"],
            if github_better { "github" } else { "mai" }
        );
        assert!(state.events.contains(&"connect:mai".into()));
        assert!(state.events.contains(&"connect:github".into()));
        assert!(
            !state
                .events
                .iter()
                .any(|event| event == "paths" || event == "device-login" || event == "launch")
        );
    }
}

#[tokio::test]
async fn automatic_selection_honors_custom_priority_and_never_chooses_an_unranked_model() {
    let fixture = Fixture::new();
    {
        let mut state = fixture.services.state.lock().unwrap();
        state.mai_models = vec!["second".into()];
        state.github_models = vec!["first".into()];
    }
    assert_eq!(
        fixture
            .run(&[
                "models",
                "--provider",
                "auto",
                "--preferred-model",
                "first",
                "--preferred-model",
                "second"
            ])
            .await
            .unwrap(),
        0
    );
    assert!(
        fixture
            .output()
            .contains("Automatic selection: first via github")
    );
    assert_eq!(
        fixture
            .run(&["models", "--provider", "auto"])
            .await
            .unwrap_err()
            .code,
        "no_preferred_model"
    );
    assert!(!fixture.events().contains(&"device-login".into()));
}

#[tokio::test]
async fn each_automatic_probe_has_one_absolute_budget_across_connect_and_catalog() {
    let fixture = Fixture::new();
    {
        let mut state = fixture.services.state.lock().unwrap();
        state.connect_delay = Duration::from_millis(180);
        state.catalog_delay = Duration::from_millis(180);
    }
    let started = Instant::now();
    assert_eq!(
        fixture
            .run(&["models", "--provider", "auto", "--probe-timeout", "0.25"])
            .await
            .unwrap_err()
            .code,
        "no_preferred_model"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(fixture.output().contains("mai unavailable"));
    assert!(fixture.output().contains("github unavailable"));
    assert!(!fixture.events().contains(&"device-login".into()));
}

#[tokio::test]
async fn missing_personal_account_is_reported_without_a_login_prompt_or_account_fallback() {
    let fixture = Fixture::new();
    fixture.services.state.lock().unwrap().stored = None;
    assert_eq!(
        fixture
            .run(&["models", "--provider", "auto"])
            .await
            .unwrap(),
        0
    );
    assert!(fixture.output().contains("github unavailable"));
    assert!(fixture.output().contains("via mai"));
    assert!(!fixture.events().contains(&"device-login".into()));
    assert!(!fixture.events().contains(&"verify".into()));
    assert!(!fixture.events().contains(&"connect:github".into()));
}

#[tokio::test]
async fn occupied_port_fails_before_any_provider_credential_configuration_or_app_operation() {
    let fixture = Fixture::new();
    let owned = listener::bind_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = owned.local_addr().unwrap().port().to_string();
    let error = fixture.run(&["auto", "--port", &port]).await.unwrap_err();
    assert_eq!(error.code, "listener_unavailable");
    assert!(error.message.contains("No process was stopped"));
    assert!(fixture.events().is_empty());
    assert!(fixture.output().is_empty());
}

#[tokio::test]
async fn automatic_setup_uses_the_owned_ephemeral_port_and_launches_only_after_identity_and_models()
{
    let fixture = Fixture::new();
    let original = b"# preserve\r\nmodel = 'old'\r\nmodel_reasoning_effort = 'ultra'\r\n[mcp_servers.fixture]\r\ncommand = 'keep'\r\n";
    fixture.write_codex(original);
    let cancellation = CancellationToken::new();
    fixture.services.state.lock().unwrap().cancel_on_launch = Some(cancellation.clone());
    assert_eq!(
        cli::run_with(
            &fixture.services,
            ["adapter", "auto", "--port", "0"],
            cancellation
        )
        .await
        .unwrap(),
        0
    );
    let state = fixture.services.state.lock().unwrap();
    let address = state.ready_address.unwrap();
    assert_ne!(address.port(), 0);
    assert_eq!(Some(address), state.bound_address);
    let order = |name: &str| state.events.iter().position(|event| event == name).unwrap();
    assert!(order("bound") < order("connect:mai"));
    assert!(order("bound") < order("store:read"));
    assert!(order("serve") < order("http:/health"));
    assert!(order("http:/health") < order("http:/v1/models"));
    assert!(order("http:/v1/models") < order("ready"));
    assert!(order("ready") < order("launch"));
    assert!(order("launch") < order("server-stopped"));
    drop(state);
    let settings = adapter_app::config::read_codex_settings(&fixture.services.paths).unwrap();
    assert_eq!(settings.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(settings.reasoning_effort.as_deref(), Some("max"));
    assert!(
        String::from_utf8(fixture.codex())
            .unwrap()
            .contains(&address.port().to_string())
    );
    assert!(
        String::from_utf8(fixture.codex())
            .unwrap()
            .contains("[mcp_servers.fixture]")
    );
    assert_eq!(
        std::fs::read(fixture.services.paths.backup_dir.join("codex.original")).unwrap(),
        original
    );
    assert!(
        !fixture
            .services
            .paths
            .claude_settings
            .as_ref()
            .unwrap()
            .exists()
    );
}

#[tokio::test]
async fn automatic_restart_preserves_compatible_external_edits_but_rejects_required_writes() {
    for change_model in [false, true] {
        let fixture = Fixture::new();
        fixture.write_codex(b"model = 'old'\n");
        let first = CancellationToken::new();
        fixture.services.state.lock().unwrap().cancel_on_launch = Some(first.clone());
        assert_eq!(
            cli::run_with(&fixture.services, ["adapter", "auto", "--port", "0"], first)
                .await
                .unwrap(),
            0
        );
        let address = fixture
            .services
            .state
            .lock()
            .unwrap()
            .ready_address
            .unwrap();
        let manifest_path = fixture.services.paths.backup_dir.join("manifest.json");
        let original_path = fixture.services.paths.backup_dir.join("codex.original");
        let manifest = std::fs::read(&manifest_path).unwrap();
        let original = std::fs::read(&original_path).unwrap();
        let mut edited = String::from_utf8(fixture.codex()).unwrap();
        edited.push_str(
            "\n# An independent client preference\n[features]\nfixture_preference = true\n",
        );
        if change_model {
            let mut document = edited.parse::<toml_edit::DocumentMut>().unwrap();
            document["model"] = toml_edit::value("gpt-5.6-sol");
            edited = document.to_string();
        }
        fixture.write_codex(edited.as_bytes());

        let second = CancellationToken::new();
        fixture.services.state.lock().unwrap().cancel_on_launch = Some(second.clone());
        let port = address.port().to_string();
        let result = cli::run_with(
            &fixture.services,
            ["adapter", "auto", "--port", &port],
            second,
        )
        .await;
        let launches = fixture
            .events()
            .iter()
            .filter(|event| *event == "launch")
            .count();
        if change_model {
            assert_eq!(result.unwrap_err().code, "configuration_conflict");
            assert_eq!(launches, 1);
        } else {
            assert_eq!(result.unwrap(), 0);
            assert_eq!(launches, 2);
            assert!(fixture.output().contains("reuse compatible settings"));
            assert!(fixture.output().contains("Configuration backup warning"));
        }
        assert_eq!(fixture.codex(), edited.as_bytes());
        assert_eq!(std::fs::read(&manifest_path).unwrap(), manifest);
        assert_eq!(std::fs::read(&original_path).unwrap(), original);
        assert!(
            !fixture
                .services
                .paths
                .backup_dir
                .join("native-configuration-journal.json")
                .exists()
        );
    }
}

#[tokio::test]
async fn manual_serve_is_headless_and_cancellation_drains_the_owned_server() {
    for host in ["127.0.0.1", "::1"] {
        let fixture = Fixture::new();
        let cancellation = CancellationToken::new();
        fixture.services.state.lock().unwrap().cancel_on_ready = Some(cancellation.clone());
        assert_eq!(
            cli::run_with(
                &fixture.services,
                ["adapter", "serve", "--host", host, "--port", "0"],
                cancellation
            )
            .await
            .unwrap(),
            0
        );
        let events = fixture.events();
        assert!(!events.iter().any(|event| event == "paths"
            || event == "launch"
            || event.starts_with("apps:")
            || event.starts_with("store:")));
        assert!(events.contains(&"server-stopped".into()));
        assert!(
            fixture
                .services
                .state
                .lock()
                .unwrap()
                .ready_address
                .unwrap()
                .ip()
                .is_loopback()
        );
        assert_eq!(std::fs::read_dir(fixture.root.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn readiness_identity_or_model_mismatch_prevents_launch_without_fallback() {
    for bad_health in [false, true] {
        let fixture = Fixture::new();
        {
            let mut state = fixture.services.state.lock().unwrap();
            state.bad_health = bad_health;
            state.missing_local_model = !bad_health;
        }

        assert!(fixture.run(&["auto", "--port", "0"]).await.is_err());
        let events = fixture.events();
        assert!(!events.contains(&"launch".into()));
        assert!(events.contains(&"server-stopped".into()));
        assert_eq!(
            events
                .iter()
                .filter(|event| *event == "connect:mai")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| *event == "connect:github")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn cancelling_pending_activation_still_drains_the_owned_foreground_server() {
    let fixture = Fixture::new();
    let cancellation = CancellationToken::new();
    {
        let mut state = fixture.services.state.lock().unwrap();
        state.cancel_on_launch = Some(cancellation.clone());
        state.pending_launch = true;
    }
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        cli::run_with(
            &fixture.services,
            ["adapter", "auto", "--port", "0"],
            cancellation,
        ),
    )
    .await
    .unwrap();
    assert_eq!(result.unwrap_err().status, 499);
    assert!(fixture.events().contains(&"server-stopped".into()));
}

#[tokio::test]
async fn automatic_configuration_rejects_effective_overrides_without_rewriting_them() {
    for original in [
        b"model_provider = 'custom'\n[model_providers.custom]\napi_key = 'fixture-secret'\n"
            .as_slice(),
        b"[model_providers.openai]\nbase_url = 'https://old.invalid'\napi_key = 'fixture-secret'\n",
        b"profile = 'custom'\n[profiles.custom]\nmodel = 'old'\n",
        b"model_context_window = 999999\n",
        b"model_auto_compact_token_limit = 999999\n",
    ] {
        let fixture = Fixture::new();
        fixture.write_codex(original);
        assert_eq!(
            fixture
                .run(&["auto", "--port", "0"])
                .await
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(fixture.codex(), original);
        assert!(!fixture.services.paths.backup_dir.exists());
        assert!(!fixture.events().contains(&"serve".into()));
        assert!(!fixture.events().contains(&"launch".into()));
        assert!(!fixture.output().contains("fixture-secret"));
    }
}

#[tokio::test]
async fn manual_launch_checks_the_configured_model_without_silently_selecting_another() {
    let fixture = Fixture::new();
    let original = b"model = 'not-advertised'\n";
    fixture.write_codex(original);
    let error = fixture
        .run(&[
            "mai", "--port", "0", "--setup", "--client", "codex", "--launch", "codex",
        ])
        .await
        .unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(fixture.codex(), original);
    assert!(!fixture.events().contains(&"launch".into()));
}

#[tokio::test]
async fn explicit_force_switches_only_the_top_level_provider_and_preserves_its_table() {
    let fixture = Fixture::new();
    let original =
        b"model_provider = 'custom'\n[model_providers.custom]\napi_key = 'fixture-secret'\n";
    fixture.write_codex(original);
    let cancellation = CancellationToken::new();
    fixture.services.state.lock().unwrap().cancel_on_launch = Some(cancellation.clone());
    assert_eq!(
        cli::run_with(
            &fixture.services,
            ["adapter", "auto", "--port", "0", "--force-openai-provider"],
            cancellation
        )
        .await
        .unwrap(),
        0
    );
    let text = String::from_utf8(fixture.codex()).unwrap();
    assert!(text.contains("[model_providers.custom]"));
    assert!(text.contains("api_key = 'fixture-secret'"));
    assert_eq!(
        adapter_app::config::read_codex_settings(&fixture.services.paths)
            .unwrap()
            .provider,
        "openai"
    );
    assert!(!fixture.output().contains("fixture-secret"));
}

#[tokio::test]
async fn setup_restore_recover_and_dry_run_are_client_only_and_preserve_exact_originals() {
    let fixture = Fixture::new();
    let original = b"# exact CRLF\r\nmodel = 'keep'\r\nmodel_reasoning_effort = 'high'\r\n[mcp_servers.fixture]\r\ncommand = 'keep'\r\n";
    fixture.write_codex(original);
    assert_eq!(
        fixture
            .run(&["setup", "--client", "codex", "--port", "5099", "--dry-run"])
            .await
            .unwrap(),
        0
    );
    assert_eq!(fixture.codex(), original);
    assert!(!fixture.services.paths.backup_dir.exists());
    assert_eq!(
        fixture
            .run(&["setup", "--client", "codex", "--port", "5099"])
            .await
            .unwrap(),
        0
    );
    assert_ne!(fixture.codex(), original);
    assert_eq!(
        fixture
            .run(&["setup", "--client", "codex", "--port", "5100"])
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        std::fs::read(fixture.services.paths.backup_dir.join("codex.original")).unwrap(),
        original
    );
    assert_eq!(
        fixture
            .run(&["restore", "--client", "codex"])
            .await
            .unwrap(),
        0
    );
    assert_eq!(fixture.codex(), original);
    assert_eq!(
        fixture
            .run(&["recover", "--client", "codex", "--dry-run"])
            .await
            .unwrap(),
        0
    );
    assert!(fixture.events().iter().all(|event| event == "paths"));
    assert!(
        !fixture
            .services
            .paths
            .claude_settings
            .as_ref()
            .unwrap()
            .exists()
    );
}

#[tokio::test]
async fn unselected_explicit_client_paths_are_not_inspected_by_recovery() {
    let fixture = Fixture::new();
    let invalid = fixture.root.path().join("invalid-claude.json");
    std::fs::write(&invalid, b"not JSON").unwrap();
    let args = [
        OsString::from("adapter"),
        OsString::from("recover"),
        OsString::from("--client"),
        OsString::from("codex"),
        OsString::from("--claude-settings"),
        invalid.into_os_string(),
    ];
    assert_eq!(
        cli::run_with(&fixture.services, args, CancellationToken::new())
            .await
            .unwrap(),
        0
    );
    assert!(fixture.events().iter().all(|event| event == "paths"));
}

#[tokio::test]
async fn doctor_reports_effective_overrides_without_loading_credentials_or_mutating_files() {
    let fixture = Fixture::new();
    let original = b"profile = 'active'\n[profiles.active]\nmodel = 'fixture-profile-model'\n[model_providers.openai]\nbase_url = 'https://old.invalid'\napi_key = 'do-not-print-fixture-secret'\n";
    fixture.write_codex(original);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = request(&mut stream).await.unwrap();
        reply(
            &mut stream,
            "application/json",
            br#"{"application":"github-adapter","provider":"mai","status":"ok"}"#,
        )
        .await
        .unwrap();
    });
    assert_eq!(
        fixture
            .run(&["doctor", "--client", "codex", "--port", &port])
            .await
            .unwrap(),
        1
    );
    task.await.unwrap();
    assert_eq!(fixture.codex(), original);
    assert!(!fixture.services.paths.backup_dir.exists());
    assert!(fixture.events().iter().all(|event| event == "paths"));
    let output = fixture.output();
    assert!(output.contains("openai provider table yes"));
    assert!(output.contains("active profile fields"));
    assert!(output.contains("Local health: GitHub Adapter responds"));
    assert!(!output.contains("do-not-print-fixture-secret"));
    assert!(!output.contains("fixture-profile-model"));
}

async fn http_json(address: SocketAddr, method: &str, path: &str, body: &[u8]) -> Value {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    ).as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    assert!(bytes.starts_with(b"HTTP/1.1 200"));
    serde_json::from_slice(&bytes[end + 4..]).unwrap()
}

#[tokio::test]
async fn native_foreground_cli_serves_real_api_routes_against_only_a_synthetic_mai_proxy() {
    let fixture = Fixture::new();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", proxy.local_addr().unwrap());
    let cache: PathBuf = fixture.root.path().join("models_cache.json");
    std::fs::write(
        &cache,
        serde_json::to_vec(&json!({"models":[model("fixture-native")]})).unwrap(),
    )
    .unwrap();
    let proxy_stop = CancellationToken::new();
    let stop_proxy = proxy_stop.clone();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let proxy_task = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = stop_proxy.cancelled() => break,
                accepted = proxy.accept() => accepted.unwrap(),
            };
            let (mut stream, _) = accepted;
            let bytes = request(&mut stream).await.unwrap();
            let first = String::from_utf8_lossy(&bytes)
                .lines()
                .next()
                .unwrap()
                .to_owned();
            recorded.lock().unwrap().push(first.clone());
            if first.starts_with("POST ") {
                let resource = json!({
                    "type":"response.completed",
                    "response": {
                        "id":"fixture-response", "object":"response", "created_at":1,
                        "model":"fixture-native", "status":"completed",
                        "output":[{"id":"fixture-message","type":"message","role":"assistant","status":"completed",
                            "content":[{"type":"output_text","text":"offline fixture output","annotations":[]}]}],
                        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                    }
                });
                reply(
                    &mut stream,
                    "text/event-stream",
                    format!("data: {resource}\n\n").as_bytes(),
                )
                .await
                .unwrap();
            } else if first.contains("/models ") {
                reply(
                    &mut stream,
                    "application/json",
                    br#"{"data":[{"id":"fixture-native"}]}"#,
                )
                .await
                .unwrap();
            } else {
                reply(
                    &mut stream,
                    "application/json",
                    br#"{"message":"MAI LLM Proxy server is running"}"#,
                )
                .await
                .unwrap();
            }
        }
    });
    let (sender, ready) = oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let services = NativeServices::with_ready_observer(move |address| {
        if let Some(sender) = sender.lock().unwrap().take() {
            let _ = sender.send(address);
        }
    });
    let cancellation = CancellationToken::new();
    let server_cancel = cancellation.clone();
    let args = vec![
        OsString::from("adapter"),
        OsString::from("mai"),
        OsString::from("--port"),
        OsString::from("0"),
        OsString::from("--upstream"),
        upstream.into(),
        OsString::from("--models-cache"),
        cache.into_os_string(),
    ];
    let server = tokio::spawn(async move { cli::run_with(&services, args, server_cancel).await });
    let address = tokio::time::timeout(Duration::from_secs(10), ready)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        http_json(address, "GET", "/health", b"").await["engine"],
        "rust"
    );
    assert_eq!(
        http_json(address, "GET", "/v1/models", b"").await["data"][0]["id"],
        "fixture-native"
    );
    let response = http_json(
        address,
        "POST",
        "/v1/responses",
        br#"{"model":"fixture-native","input":"offline fixture","stream":false}"#,
    )
    .await;
    assert_eq!(
        response["output"][0]["content"][0]["text"],
        "offline fixture output"
    );
    cancellation.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(12), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        0
    );
    proxy_stop.cancel();
    proxy_task.await.unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("POST "))
            .count(),
        1
    );
    assert!(
        !fixture
            .services
            .paths
            .codex_config
            .as_ref()
            .unwrap()
            .exists()
    );
    assert!(!fixture.services.paths.backup_dir.exists());
}
