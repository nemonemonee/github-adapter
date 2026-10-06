//! One selected provider/account, with owned transport, catalogs, and history.

#[cfg(test)]
mod catalog_tests;
mod messages;
mod models;
mod parity;
mod recovery;
mod request;
mod stream;

use crate::auth::{AuthClient, CopilotAuth, CopilotSession, Credential};
use crate::context::{CancelOnDrop, RequestContext};
use crate::history::{HistoryLimits, ResponseHistory};
use crate::selection::{AccountScope, Provider, SelectedRoute};
use crate::transport::{self, Redaction};
use adapter_protocol::chat::{chat_to_response, response_events, responses_to_chat};
use adapter_protocol::identity::NativeStream;
use adapter_protocol::sse::{SseDecoder, SseEvent, StreamLimits};
use adapter_protocol::{AdapterError, Result, Value, compaction};
use futures_util::StreamExt;
use models::Catalog;
use request::{
    RequestKind, cap_tokens, contains_image, initiator, remove_unsupported_image_tools,
    validate_github_controls,
};
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Response};
use serde_json::json;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use stream::{
    DirectChat, Mode, OutputBudget, ResourceUse, compressed, is_terminal, redacted_stream,
    resource_value, terminal_value, validate_chat_completion,
};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

pub use stream::EventStream;

pub enum BackendConfig {
    Github {
        credential: Credential,
    },
    Mai {
        upstream: String,
        models_cache: PathBuf,
    },
}

pub struct Backend {
    scope: AccountScope,
    source: Source,
    history: ResponseHistory,
    catalog: Mutex<Option<CachedCatalog>>,
    display_catalog: RwLock<Option<CachedCatalog>>,
    upstream: RwLock<String>,
    review_model: RwLock<Option<String>>,
    recovery: recovery::Recovery,
}

enum Source {
    Github(Arc<CopilotAuth>),
    Mai {
        client: Client,
        endpoint: Url,
        models_cache: PathBuf,
    },
}

#[derive(Clone)]
struct CachedCatalog {
    endpoint: String,
    expires_at: Instant,
    catalog: Arc<Catalog>,
}

#[derive(Clone)]
enum Binding {
    Github {
        auth: Arc<CopilotAuth>,
        session: Arc<CopilotSession>,
    },
    Mai {
        client: Client,
        endpoint: Url,
    },
}

impl Binding {
    fn endpoint(&self) -> &Url {
        match self {
            Self::Github { session, .. } => &session.endpoint,
            Self::Mai { endpoint, .. } => endpoint,
        }
    }

    fn client(&self) -> &Client {
        match self {
            Self::Github { session, .. } => &session.client,
            Self::Mai { client, .. } => client,
        }
    }

    fn redaction(&self) -> Redaction {
        match self {
            Self::Github { auth, session } => {
                Redaction::new(&[auth.credential.token(), session.token()])
            }
            Self::Mai { .. } => Redaction::default(),
        }
    }

    fn github(&self) -> bool {
        matches!(self, Self::Github { .. })
    }

    fn route_key(&self, native: bool) -> String {
        format!(
            "{}|{}",
            transport::endpoint_name(self.endpoint()),
            if native {
                "/responses"
            } else {
                "/chat/completions"
            }
        )
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Health,
    Models,
    Responses,
    Chat,
}

#[derive(Clone)]
struct Prepared {
    trace: recovery::Trace,
    request: Value,
    outgoing: Value,
    binding: Binding,
    route: SelectedRoute,
    history_route: String,
    native: bool,
}

struct StreamRun {
    payload: Value,
    prepared: Prepared,
    mode: Mode,
    body: transport::ByteStream,
    context: RequestContext,
    guard: CancelOnDrop,
    verify_eof: bool,
}

impl fmt::Debug for Backend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Backend")
            .field("health", &self.health())
            .finish_non_exhaustive()
    }
}

impl Backend {
    pub async fn connect(config: BackendConfig, ctx: RequestContext) -> Result<Arc<Self>> {
        let _guard = ctx.cancel_on_drop();
        ctx.check()?;
        match config {
            BackendConfig::Github { credential } => {
                Self::github(credential, AuthClient::new()?, &ctx).await
            }
            BackendConfig::Mai {
                upstream,
                models_cache,
            } => {
                let endpoint = transport::endpoint(&upstream)?;
                let name = transport::endpoint_name(&endpoint);
                let scope = AccountScope::new(Provider::Mai, name.clone())?;
                let backend = Arc::new(Self {
                    history: ResponseHistory::new(scope.clone(), HistoryLimits::default())?,
                    scope,
                    source: Source::Mai {
                        client: transport::client(transport::loopback(&endpoint))?,
                        endpoint,
                        models_cache,
                    },
                    catalog: Mutex::new(None),
                    display_catalog: RwLock::new(None),
                    upstream: RwLock::new(name),
                    review_model: RwLock::new(None),
                    recovery: recovery::Recovery::default(),
                });
                let binding = backend.binding(&ctx).await?;
                let response = backend
                    .open(&binding, Operation::Health, None, false, &ctx, None)
                    .await?;
                let health = transport::read_json(
                    response,
                    &ctx,
                    transport::HEALTH_BYTES,
                    "MAI proxy health check",
                )
                .await?;
                if health.get("message").and_then(Value::as_str)
                    != Some("MAI LLM Proxy server is running")
                {
                    return Err(AdapterError::upstream(
                        "Configured upstream is not the MAI LLM Proxy.",
                    ));
                }
                backend.models(&ctx, false).await?;
                Ok(backend)
            }
        }
    }

    async fn github(
        credential: Credential,
        http: AuthClient,
        context: &RequestContext,
    ) -> Result<Arc<Self>> {
        let credential = http
            .verify(credential.token(), Some(&credential.login), context)
            .await?;
        let scope = AccountScope::new(Provider::Github, credential.login.clone())?;
        let backend = Arc::new(Self {
            history: ResponseHistory::new(scope.clone(), HistoryLimits::default())?,
            scope,
            source: Source::Github(Arc::new(CopilotAuth::new(credential, http))),
            catalog: Mutex::new(None),
            display_catalog: RwLock::new(None),
            upstream: RwLock::new("https://api.githubcopilot.com".into()),
            review_model: RwLock::new(None),
            recovery: recovery::Recovery::default(),
        });
        backend.models(context, false).await?;
        Ok(backend)
    }

    pub fn scope(&self) -> &AccountScope {
        &self.scope
    }
    pub fn history(&self) -> &ResponseHistory {
        &self.history
    }

    pub fn health(&self) -> Value {
        let (provider, account) = match &self.source {
            Source::Github(auth) => (
                "github",
                json!(Redaction::new(&[auth.credential.token()]).text(&auth.credential.login)),
            ),
            Source::Mai { .. } => ("mai", Value::Null),
        };
        json!({
            "status": "ok", "message": "GitHub Adapter is running",
            "application": "github-adapter", "version": "2.0", "engine": "rust",
            "build_version": env!("CARGO_PKG_VERSION"),
            "response_failures": self.recovery.snapshot(),
            "encrypted_state_recovery": self.recovery.enabled(),
            "provider": provider, "account": account,
            "upstream": self.upstream.read().map(|upstream| upstream.clone()).unwrap_or_else(|_| "unavailable".into()),
        })
    }

    fn redact(&self, error: AdapterError) -> AdapterError {
        match &self.source {
            Source::Github(auth) => Redaction::new(&[auth.credential.token()]).error(error),
            Source::Mai { .. } => error,
        }
    }

    async fn binding(&self, context: &RequestContext) -> Result<Binding> {
        context.check()?;
        let binding = match &self.source {
            Source::Github(auth) => Binding::Github {
                auth: auth.clone(),
                session: auth.session(context).await.inspect_err(|error| {
                    if matches!(error.status, 401 | 403)
                        && let Ok(mut display) = self.display_catalog.write()
                    {
                        *display = None;
                    }
                })?,
            },
            Source::Mai {
                client, endpoint, ..
            } => Binding::Mai {
                client: client.clone(),
                endpoint: endpoint.clone(),
            },
        };
        let name = binding
            .redaction()
            .text(&transport::endpoint_name(binding.endpoint()));
        let mut upstream = self.upstream.write().map_err(|_| {
            AdapterError::new(
                500,
                "backend_unavailable",
                "The selected backend state is unavailable.",
            )
        })?;
        if *upstream != name
            && let Ok(mut display) = self.display_catalog.write()
        {
            *display = None;
        }
        *upstream = name;
        Ok(binding)
    }

    async fn open(
        &self,
        binding: &Binding,
        operation: Operation,
        payload: Option<&Value>,
        streaming: bool,
        context: &RequestContext,
        prepared: Option<&Prepared>,
    ) -> Result<Response> {
        let redaction = binding.redaction();
        let result = async {
            context.check()?;
            let (path, post) = match (binding.github(), operation) {
                (true, Operation::Health) => {
                    return Err(AdapterError::invalid(
                        "This endpoint is not exposed by personal GitHub mode.",
                    ));
                }
                (true, Operation::Models) => ("/models", false),
                (true, Operation::Responses) => ("/responses", true),
                (true, Operation::Chat) => ("/chat/completions", true),
                (false, Operation::Health) => ("/", false),
                (false, Operation::Models) => ("/v1/models", false),
                (false, Operation::Responses) => ("/v1/responses", true),
                (false, Operation::Chat) => ("/v1/chat/completions", true),
            };
            if post != payload.is_some() {
                return Err(AdapterError::new(
                    500,
                    "invalid_operation",
                    "Invalid upstream operation framing.",
                ));
            }
            let url = transport::append_path(binding.endpoint(), path)?;
            let mut request = if post {
                binding.client().post(url)
            } else {
                binding.client().get(url)
            };
            request = request
                .header(
                    ACCEPT,
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .header(ACCEPT_ENCODING, "identity")
                .timeout(transport::timeout(
                    context,
                    (!post).then_some(transport::CONTROL_TIMEOUT),
                )?);
            if let Some(payload) = payload {
                request = request
                    .header(CONTENT_TYPE, "application/json")
                    .body(transport::encode(payload, transport::INFERENCE_BYTES)?);
            }
            if let Binding::Github { session, .. } = binding {
                session.check()?;
                request = request
                    .header(AUTHORIZATION, transport::bearer("Bearer", session.token())?)
                    .header(
                        "User-Agent",
                        format!("GitHubCopilotChat/0.26.7 {}", transport::USER_AGENT),
                    )
                    .header("Copilot-Integration-Id", "vscode-chat")
                    .header("Editor-Version", "vscode/1.137.0")
                    .header("Editor-Plugin-Version", "copilot-chat/0.26.7")
                    .header("Openai-Intent", "conversation-panel")
                    .header("X-GitHub-Api-Version", "2025-04-01")
                    .header("X-Vscode-User-Agent-Library-Version", "electron-fetch")
                    .header("X-Request-Id", transport::request_id())
                    .header(
                        "X-Initiator",
                        if matches!(operation, Operation::Chat) {
                            initiator(payload)
                        } else {
                            "user"
                        },
                    );
                if payload.is_some_and(contains_image) {
                    request = request.header("Copilot-Vision-Request", "true");
                }
            }
            let response = transport::send(
                request,
                context,
                if binding.github() {
                    "Personal Copilot"
                } else {
                    "MAI"
                },
            )
            .await?;
            let status = response.status().as_u16();
            if let Some(prepared) = prepared {
                prepared
                    .trace
                    .set_request_id(response.headers(), &redaction);
            }
            if status == 401
                && let Binding::Github { auth, session } = binding
            {
                auth.invalidate(session)?;
            }
            if binding.github()
                && matches!(operation, Operation::Models)
                && matches!(status, 401 | 403)
            {
                // Rejected metadata cannot become a stale success if its error
                // body stalls after the authoritative response headers arrive.
                if let Ok(mut display) = self.display_catalog.write() {
                    *display = None;
                }
                context.check()?;
                return Err(AdapterError::new(
                    status,
                    "upstream_error",
                    format!("The selected upstream returned HTTP {status}."),
                ));
            }
            if !(200..300).contains(&status) {
                let payload = transport::read_json(
                    response,
                    context,
                    transport::AUTH_BYTES,
                    "Selected upstream",
                )
                .await?;
                if let Some(prepared) =
                    prepared.filter(|_| matches!(operation, Operation::Responses))
                {
                    self.recovery
                        .observe_failure(prepared, "http_error", &payload, &redaction);
                }
                return Err(AdapterError::new(
                    transport::error_status(status),
                    "upstream_error",
                    transport::message(
                        &payload,
                        &format!("The selected upstream returned HTTP {status}."),
                    ),
                ));
            }
            Ok(response)
        }
        .await;
        result.map_err(|error| redaction.error(error))
    }

    async fn open_prepared(
        &self,
        prepared: &Prepared,
        streaming: bool,
        context: &RequestContext,
    ) -> Result<Response> {
        // Keep the selected binding, payload and trace together for every inference.
        self.open(
            &prepared.binding,
            if prepared.native {
                Operation::Responses
            } else {
                Operation::Chat
            },
            Some(&prepared.outgoing),
            streaming,
            context,
            Some(prepared),
        )
        .await
    }

    async fn read_cache(&self, context: &RequestContext) -> Result<Value> {
        let Source::Mai { models_cache, .. } = &self.source else {
            return Err(AdapterError::new(
                500,
                "invalid_operation",
                "GitHub catalogs never read the Codex cache.",
            ));
        };
        let raw = context
            .bounded(async {
                let file = tokio::fs::File::open(models_cache).await.map_err(|_| {
                    AdapterError::upstream("The configured MAI model cache could not be opened.")
                })?;
                if !file
                    .metadata()
                    .await
                    .map_err(|_| {
                        AdapterError::upstream("The MAI model cache could not be inspected.")
                    })?
                    .is_file()
                {
                    return Err(AdapterError::upstream(
                        "The MAI model cache must be a regular file.",
                    ));
                }
                let mut raw = Vec::new();
                file.take(transport::CACHE_BYTES as u64 + 1)
                    .read_to_end(&mut raw)
                    .await
                    .map_err(|_| {
                        AdapterError::upstream("The configured MAI model cache could not be read.")
                    })?;
                Ok(raw)
            })
            .await?;
        adapter_protocol::json::decode(&raw, transport::CACHE_BYTES).map_err(|_| {
            AdapterError::upstream("The MAI model cache contains malformed or oversized JSON.")
        })
    }

    async fn models(
        &self,
        context: &RequestContext,
        force: bool,
    ) -> Result<(Arc<Catalog>, Binding)> {
        let binding = self.binding(context).await?;
        let redaction = binding.redaction();
        let result = async {
            let endpoint = transport::endpoint_name(binding.endpoint());
            let mut cached = context
                .bounded(async { Ok(self.catalog.lock().await) })
                .await?;
            if !force
                && let Some(cached) = cached.as_ref().filter(|cached| {
                    cached.endpoint == endpoint && Instant::now() < cached.expires_at
                })
            {
                return Ok((cached.catalog.clone(), binding.clone()));
            }
            *cached = None;
            let catalog = async {
                let cache = if binding.github() {
                    None
                } else {
                    Some(self.read_cache(context).await?)
                };
                let response = self
                    .open(&binding, Operation::Models, None, false, context, None)
                    .await?;
                if transport::content_type(&response)? == "text/event-stream" {
                    return Err(AdapterError::upstream(
                        "The selected provider returned a stream instead of a JSON model list.",
                    ));
                }
                let payload = transport::read_json(
                    response,
                    context,
                    transport::MODEL_BYTES,
                    "Model discovery",
                )
                .await?;
                Ok(Arc::new(match &self.source {
                    Source::Github(auth) => models::github(payload, &auth.credential.login)?,
                    Source::Mai { .. } => models::mai(
                        cache.ok_or_else(|| {
                            AdapterError::upstream("The MAI model cache is missing.")
                        })?,
                        payload,
                    )?,
                }))
            }
            .await
            .inspect_err(|error| {
                if matches!(error.status, 401 | 403)
                    && let Ok(mut display) = self.display_catalog.write()
                {
                    *display = None;
                }
            })?;
            let fresh = CachedCatalog {
                endpoint,
                expires_at: Instant::now() + Duration::from_secs(300),
                catalog: catalog.clone(),
            };
            if let Ok(mut display) = self.display_catalog.write() {
                *display = Some(fresh.clone());
            }
            *cached = Some(fresh);
            Ok((catalog, binding))
        }
        .await;
        result.map_err(|error| redaction.error(error))
    }

    pub async fn model_catalog(&self, ctx: &RequestContext) -> Result<Value> {
        let has_previous = self.scope.provider == Provider::Github
            && self
                .display_catalog
                .read()
                .is_ok_and(|display| display.is_some());
        let mut refresh = ctx.child();
        let _guard = refresh.cancel_on_drop();
        if has_previous {
            refresh.deadline = refresh
                .deadline
                .min(Instant::now() + Duration::from_millis(1500));
        }
        match self
            .models(&refresh, self.scope.provider == Provider::Mai)
            .await
        {
            Ok((catalog, _)) => Ok(catalog.value.clone()),
            Err(error) => {
                ctx.check()?;
                if self.scope.provider == Provider::Github
                    && (500..600).contains(&error.status)
                    && let Some(previous) = self
                        .display_catalog
                        .read()
                        .ok()
                        .and_then(|display| display.clone())
                        .filter(|cached| {
                            self.upstream
                                .read()
                                .is_ok_and(|endpoint| *endpoint == cached.endpoint)
                        })
                {
                    // This fallback is only the model-picker display. Its expired
                    // entry cannot authorize inference or move to a new endpoint.
                    let mut value = previous.catalog.value.clone();
                    value["catalog_source"] = json!("last_known_good");
                    value["catalog_stale"] = json!(true);
                    return Ok(value);
                }
                Err(self.redact(error))
            }
        }
    }

    async fn prepare(
        &self,
        payload: &Value,
        kind: RequestKind,
        context: &RequestContext,
    ) -> Result<Prepared> {
        context.check()?;
        let requested = request::validate_shape(payload, kind, self.scope.provider)?;
        let github = self.scope.provider == Provider::Github;
        let alias = if !github && !kind.is_chat() {
            match requested {
                "gpt-5.6-sol-max" => Some("gpt-5.6-sol"),
                "gpt-5.6-luna-max" => Some("gpt-5.6-luna"),
                _ => None,
            }
        } else {
            None
        };
        let (catalog, binding) = self.models(context, false).await?;
        let redaction = binding.redaction();
        let result = (|| {
            let review = requested == "codex-auto-review";
            if review && kind.is_chat() {
                return Err(AdapterError::invalid(
                    "Auto-review requires Responses, not direct Chat.",
                ));
            }
            let review_target = review
                .then(|| self.review_target(&catalog, github))
                .transpose()?;
            let model = models::select(
                &catalog,
                review_target.as_deref().or(alias).unwrap_or(requested),
                if review {
                    &Value::Null
                } else {
                    &payload["service_tier"]
                },
                github,
            )?
            .clone();
            if kind.is_chat() && github && !model.direct_chat() {
                return Err(AdapterError::invalid(
                    "This model does not advertise the Chat Completions endpoint.",
                ));
            }
            let native = !kind.is_chat() && (!github || model.native());
            if kind == RequestKind::Compaction && !native {
                return Err(AdapterError::invalid(
                    "Compaction requires a model with native Responses support.",
                ));
            }
            let history_route = binding.route_key(native);
            let mut request = if kind.is_chat() || !github {
                // MAI owns its remote conversation and previous_response_id. Do not
                // duplicate that context by also replaying a local materialization.
                payload.clone()
            } else {
                self.history.expand(payload, &history_route, &model.id)?
            };
            request["model"] = json!(model.id);
            if review {
                request.as_object_mut().unwrap().remove("service_tier");
            }
            remove_unsupported_image_tools(&mut request)?;
            if github {
                let fields = request
                    .as_object_mut()
                    .ok_or_else(|| AdapterError::invalid("The request must be an object."))?;
                fields.remove("store");
                fields.remove("service_tier");
                if !kind.is_chat() {
                    fields.remove("background");
                    fields.remove("conversation");
                    fields.remove("previous_response_id");
                }
                validate_github_controls(&request, &model, kind)?;
            } else {
                if alias.is_some() {
                    let reasoning = request
                        .as_object_mut()
                        .and_then(|request| request.get_mut("reasoning"));
                    match reasoning {
                        Some(Value::Object(reasoning)) => {
                            reasoning.insert("effort".into(), json!("max"));
                        }
                        Some(Value::Null) | None => {
                            request["reasoning"] = json!({"effort": "max"});
                        }
                        Some(_) => {
                            return Err(AdapterError::invalid("reasoning must be an object."));
                        }
                    }
                    cap_tokens(&mut request, "max_output_tokens", Some(128_000))?;
                }
            }
            if kind.is_chat() {
                cap_tokens(&mut request, "max_tokens", model.output_limit)?;
                cap_tokens(&mut request, "max_completion_tokens", model.output_limit)?;
            } else {
                cap_tokens(&mut request, "max_output_tokens", model.output_limit)?;
            }
            if kind == RequestKind::Compaction {
                request = compaction::request(&request)?;
            } else {
                request["stream"] = json!(if native && github {
                    true
                } else {
                    kind.stream()
                });
            }
            let outgoing = if native || kind.is_chat() {
                request.clone()
            } else {
                let mut chat = responses_to_chat(&request, kind.stream())?;
                if !model.vendor.eq_ignore_ascii_case("openai") {
                    let fields = chat
                        .as_object_mut()
                        .ok_or_else(|| AdapterError::invalid("Invalid translated Chat request."))?;
                    if let Some(tokens) = fields.remove("max_completion_tokens") {
                        fields.insert("max_tokens".into(), tokens);
                    }
                }
                chat
            };
            transport::encode(&outgoing, transport::INFERENCE_BYTES)?;
            Ok(Prepared {
                trace: self.recovery.trace(),
                request,
                outgoing,
                binding: binding.clone(),
                route: SelectedRoute {
                    scope: self.scope.clone(),
                    model: model.id,
                },
                history_route,
                native,
            })
        })();
        result.map_err(|error| redaction.error(error))
    }

    fn annotate_response(
        &self,
        payload: &Value,
        prepared: &Prepared,
        response: &mut Value,
    ) -> Result<bool> {
        if prepared.route.scope != self.scope
            || prepared.request["model"] != prepared.route.model
            || !response
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
            || !matches!(
                response["status"].as_str(),
                Some("completed" | "incomplete")
            )
            || !response.get("output").is_some_and(Value::is_array)
            || response.get("error").is_some_and(|error| !error.is_null())
        {
            return Err(AdapterError::upstream(
                "The selected upstream did not return a valid owned terminal response.",
            ));
        }
        if self.scope.provider == Provider::Mai {
            parity::published_id(response)?;
        }
        let store = payload.get("store") != Some(&Value::Bool(false));
        response["store"] = json!(store);
        response["previous_response_id"] = payload
            .get("previous_response_id")
            .cloned()
            .unwrap_or(Value::Null);
        Ok(store)
    }

    fn retain(
        &self,
        payload: &Value,
        prepared: &Prepared,
        mut response: Value,
        context: &RequestContext,
    ) -> Result<Value> {
        context.check()?;
        let store = self.annotate_response(payload, prepared, &mut response)?;
        if store
            && !self.history.remember(
                prepared.request.clone(),
                response.clone(),
                &prepared.history_route,
            )?
        {
            response["store"] = json!(false);
        }
        Ok(response)
    }

    async fn complete_prepared(
        &self,
        payload: Value,
        prepared: Prepared,
        ctx: RequestContext,
    ) -> Result<Value> {
        let _guard = ctx.cancel_on_drop();
        let redaction = prepared.binding.redaction();
        let result = async {
            let response = self.open_prepared(&prepared, false, &ctx).await?;
            let response = if prepared.native {
                self.native_value(
                    response,
                    &ctx,
                    false,
                    self.scope.provider == Provider::Mai && payload["background"] == true,
                    &prepared,
                )
                .await?
            } else {
                if transport::content_type(&response)? == "text/event-stream" {
                    return Err(AdapterError::upstream(
                        "Copilot returned a stream instead of a buffered Chat completion.",
                    ));
                }
                chat_to_response(
                    &transport::read_json(
                        response,
                        &ctx,
                        transport::INFERENCE_BYTES,
                        "Chat completion",
                    )
                    .await?,
                    &prepared.request,
                )?
            };
            if matches!(response["status"].as_str(), Some("queued" | "in_progress")) {
                return Ok(response);
            }
            self.retain(&payload, &prepared, response, &ctx)
        }
        .await;
        result.map_err(|error| redaction.error(error))
    }

    async fn native_value(
        &self,
        response: Response,
        context: &RequestContext,
        compacting: bool,
        pending_allowed: bool,
        prepared: &Prepared,
    ) -> Result<Value> {
        if transport::content_type(&response)? != "text/event-stream" {
            let mut value = transport::read_json(
                response,
                context,
                transport::INFERENCE_BYTES,
                "Native Responses",
            )
            .await?;
            prepared.trace.observe_json(&value);
            if value.get("error").is_some_and(|error| !error.is_null()) {
                self.recovery.observe_failure(
                    prepared,
                    "response.failed",
                    &value,
                    &prepared.binding.redaction(),
                );
                return Err(AdapterError::upstream(transport::message(
                    &value,
                    "The native response failed.",
                )));
            }
            let absent_status = compacting && value.get("status").is_none();
            if absent_status {
                value["status"] = json!("completed");
            }
            let mut value = resource_value(
                value,
                if pending_allowed {
                    ResourceUse::Background
                } else {
                    ResourceUse::Inference
                },
            )?;
            if !compacting && self.scope.provider == Provider::Mai {
                parity::published_id(&value)?;
            }
            if absent_status {
                value
                    .as_object_mut()
                    .ok_or_else(|| AdapterError::upstream("Invalid compaction resource."))?
                    .remove("status");
            }
            return Ok(value);
        }
        let verify_eof = compressed(&response);
        let mut body = transport::body(
            response,
            context.clone(),
            transport::INFERENCE_BYTES,
            "Native Responses",
        )?;
        let mut framing = SseDecoder::default();
        let mut native = NativeStream::new();
        let mut terminal = None;
        while let Some(chunk) = body.next().await {
            context.check()?;
            let chunk = chunk?;
            let events = framing.push(&chunk)?;
            self.recovery.observe(prepared, &events, chunk.len());
            for event in events {
                let event = native.push(event)?;
                if is_terminal(&event) {
                    terminal = Some(terminal_value(&event)?);
                }
            }
            if terminal.is_some() && !verify_eof {
                native.finish()?;
                return terminal
                    .ok_or_else(|| AdapterError::upstream("Missing native terminal response."));
            }
        }
        let events = framing.finish()?;
        self.recovery.observe(prepared, &events, 0);
        for event in events {
            let event = native.push(event)?;
            if is_terminal(&event) {
                terminal = Some(terminal_value(&event)?);
            }
        }
        native.finish()?;
        terminal.ok_or_else(|| {
            AdapterError::upstream("The native stream ended without a terminal response.")
        })
    }

    async fn stream_prepared(
        self: Arc<Self>,
        payload: Value,
        prepared: Prepared,
        ctx: RequestContext,
    ) -> Result<EventStream> {
        let guard = ctx.cancel_on_drop();
        let redaction = prepared.binding.redaction();
        let mode = if prepared.native {
            Mode::native()
        } else {
            Mode::bridge(&prepared.request)?
        };
        let response = self.open_prepared(&prepared, true, &ctx).await?;
        if transport::content_type(&response)? != "text/event-stream" {
            if prepared.native {
                return Err(AdapterError::upstream(
                    "The selected upstream did not return a Responses event stream.",
                ));
            }
            let completion = transport::read_json(
                response,
                &ctx,
                transport::INFERENCE_BYTES,
                "Chat completion",
            )
            .await
            .map_err(|error| redaction.error(error))?;
            let converted = chat_to_response(&completion, &prepared.request)
                .map_err(|error| redaction.error(error))?;
            let events = response_events(&converted).map_err(|error| redaction.error(error))?;
            let stream: EventStream = Box::pin(async_stream::try_stream! {
                let _guard = guard;
                let mut budget = OutputBudget::default();
                for mut event in events {
                    ctx.check()?;
                    self.retain_event(&payload, &prepared, &mut event, &mut budget, &ctx)?;
                    yield event;
                }
            });
            return Ok(redacted_stream(stream, redaction));
        }
        let verify_eof = compressed(&response);
        let body = transport::body(
            response,
            ctx.clone(),
            transport::INFERENCE_BYTES,
            "Responses stream",
        )
        .map_err(|error| redaction.error(error))?;
        Ok(self.pipeline(StreamRun {
            payload,
            prepared,
            mode,
            body,
            context: ctx,
            guard,
            verify_eof,
        }))
    }

    fn retain_event(
        &self,
        payload: &Value,
        prepared: &Prepared,
        event: &mut SseEvent,
        budget: &mut OutputBudget,
        context: &RequestContext,
    ) -> Result<()> {
        if is_terminal(event) {
            let store = {
                let response = event
                    .data
                    .as_mut()
                    .and_then(|data| data.get_mut("response"))
                    .ok_or_else(|| AdapterError::upstream("Missing terminal event data."))?;
                self.annotate_response(payload, prepared, response)?
            };
            budget.event(event)?;
            context.check()?;
            let response = event
                .data
                .as_mut()
                .and_then(|data| data.get_mut("response"))
                .ok_or_else(|| AdapterError::upstream("Missing terminal event data."))?;
            if store
                && !self.history.remember(
                    prepared.request.clone(),
                    response.clone(),
                    &prepared.history_route,
                )?
            {
                response["store"] = json!(false);
                event.to_bytes()?;
                // Changing true to false adds one byte and did not retain an entry.
                budget.add(1)?;
            }
        } else {
            if payload.get("store") == Some(&Value::Bool(false))
                && let Some(response) = event
                    .data
                    .as_mut()
                    .and_then(|data| data.get_mut("response"))
                    .filter(|value| value.is_object())
            {
                response["store"] = json!(false);
            }
            budget.event(event)?;
        }
        Ok(())
    }

    fn pipeline(self: Arc<Self>, run: StreamRun) -> EventStream {
        let StreamRun {
            payload,
            prepared,
            mut mode,
            mut body,
            context,
            guard,
            verify_eof,
        } = run;
        let redaction = prepared.binding.redaction();
        let stream: EventStream = Box::pin(async_stream::try_stream! {
            let _guard = guard;
            let mut framing = SseDecoder::new(StreamLimits::default());
            let mut held = Vec::new();
            let mut budget = OutputBudget::default();
            context.check()?;
            for event in mode.start()? {
                budget.event(&event)?;
                context.check()?;
                yield event;
            }
            while let Some(chunk) = body.next().await {
                context.check()?;
                let chunk = chunk?;
                let events = framing.push(&chunk)?;
                self.recovery.observe(&prepared, &events, chunk.len());
                let events = mode.batch(events)?;
                if mode.complete() && verify_eof {
                    held.extend(events);
                    continue;
                }
                let complete = mode.complete();
                if complete { mode.finish()?; }
                for mut event in events {
                    context.check()?;
                    if mode.direct() { budget.event(&event)?; }
                    else { self.retain_event(&payload, &prepared, &mut event, &mut budget, &context)?; }
                    yield event;
                }
                if complete { return; }
            }
            let events = framing.finish()?;
            self.recovery.observe(&prepared, &events, 0);
            held.extend(mode.batch(events)?);
            mode.finish()?;
            for mut event in held {
                context.check()?;
                if mode.direct() { budget.event(&event)?; }
                else { self.retain_event(&payload, &prepared, &mut event, &mut budget, &context)?; }
                yield event;
            }
        });
        redacted_stream(stream, redaction)
    }

    pub async fn complete_chat(&self, payload: Value, ctx: RequestContext) -> Result<Value> {
        let _guard = ctx.cancel_on_drop();
        let prepared = self
            .prepare(&payload, RequestKind::Chat { stream: false }, &ctx)
            .await
            .map_err(|error| self.redact(error))?;
        let redaction = prepared.binding.redaction();
        let result = async {
            let response = self.open_prepared(&prepared, false, &ctx).await?;
            if transport::content_type(&response)? == "text/event-stream" {
                return Err(AdapterError::upstream(
                    "The selected upstream returned a stream instead of a Chat completion.",
                ));
            }
            let value = transport::read_json(
                response,
                &ctx,
                transport::INFERENCE_BYTES,
                "Chat completion",
            )
            .await?;
            validate_chat_completion(&value)?;
            Ok(value)
        }
        .await;
        result.map_err(|error| redaction.error(error))
    }

    pub async fn stream_chat(
        self: Arc<Self>,
        payload: Value,
        ctx: RequestContext,
    ) -> Result<EventStream> {
        let guard = ctx.cancel_on_drop();
        let prepared = self
            .prepare(&payload, RequestKind::Chat { stream: true }, &ctx)
            .await
            .map_err(|error| self.redact(error))?;
        let response = self.open_prepared(&prepared, true, &ctx).await?;
        if transport::content_type(&response)? != "text/event-stream" {
            return Err(AdapterError::upstream(
                "The selected upstream did not return a Chat event stream.",
            ));
        }
        let verify_eof = compressed(&response);
        let body = transport::body(
            response,
            ctx.clone(),
            transport::INFERENCE_BYTES,
            "Chat stream",
        )
        .map_err(|error| prepared.binding.redaction().error(error))?;
        Ok(self.pipeline(StreamRun {
            payload,
            prepared,
            mode: Mode::Direct(DirectChat::default()),
            body,
            context: ctx,
            guard,
            verify_eof,
        }))
    }

    pub async fn compact_responses(&self, payload: Value, ctx: RequestContext) -> Result<Value> {
        let _guard = ctx.cancel_on_drop();
        let prepared = self
            .prepare(&payload, RequestKind::Compaction, &ctx)
            .await
            .map_err(|error| self.redact(error))?;
        let redaction = prepared.binding.redaction();
        let result = async {
            let response = self.open_prepared(&prepared, false, &ctx).await?;
            compaction::response(
                &self
                    .native_value(response, &ctx, true, false, &prepared)
                    .await?,
            )
        }
        .await;
        result.map_err(|error| redaction.error(error))
    }
}

#[cfg(test)]
mod tests;
