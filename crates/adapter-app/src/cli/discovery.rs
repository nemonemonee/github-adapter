//! Provider discovery, pinned catalog identity, and advertised model compatibility.

use std::collections::HashSet;
use std::path::PathBuf;

use adapter_protocol::{AdapterError, Result, Value};
use adapter_runtime::backend::BackendConfig;
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::{self, AccountScope, AvailableProvider, Provider};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::credentials::{personal_credential, redact_error};
use super::options::{MANUAL_TIMEOUT, Options, ProviderMode};
use super::services::{Output, Services, cancelled, safe_text};

pub(super) struct Ready<B> {
    pub(super) backend: B,
    pub(super) catalog: Value,
    pub(super) models: Vec<Value>,
    pub(super) available: AvailableProvider,
}

pub(super) struct Selected<B> {
    pub(super) ready: Ready<B>,
    pub(super) model: Option<Value>,
}

fn models_cache<S: Services>(services: &S, options: &Options) -> Result<PathBuf> {
    if let Some(path) = &options.models_cache {
        if path.as_os_str().is_empty() {
            return Err(AdapterError::invalid("--models-cache cannot be empty."));
        }
        return Ok(path.clone());
    }
    let directory = if let Some(path) = services.environment("CODEX_HOME") {
        path
    } else {
        let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = services
            .environment(variable)
            .filter(|home| !home.is_empty())
            .ok_or_else(|| {
                AdapterError::invalid("Supply --models-cache or a Codex home directory.")
            })?;
        return Ok(PathBuf::from(home).join(".codex").join("models_cache.json"));
    };
    if directory.to_string_lossy().trim().is_empty() {
        return Err(AdapterError::invalid(
            "CODEX_HOME is empty; supply --models-cache.",
        ));
    }
    Ok(PathBuf::from(directory).join("models_cache.json"))
}

pub fn compatible_reasoning_effort(model: &Value, current: Option<&str>) -> Result<Option<String>> {
    let empty = Vec::new();
    let levels = match model.get("supported_reasoning_levels") {
        None => &empty,
        Some(value) => value.as_array().ok_or_else(|| {
            AdapterError::upstream("The selected model has an invalid reasoning menu.")
        })?,
    };
    let efforts = levels
        .iter()
        .map(|level| {
            level
                .get("effort")
                .and_then(Value::as_str)
                .filter(|effort| !effort.is_empty())
                .ok_or_else(|| {
                    AdapterError::upstream("The selected model has an invalid reasoning menu.")
                })
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(current) = current.filter(|current| efforts.contains(current)) {
        return Ok(Some(current.into()));
    }
    if efforts.is_empty() {
        return if current.is_none() {
            Ok(None)
        } else {
            Err(AdapterError::invalid(
                "The selected model advertises no reasoning menu. Remove the explicit reasoning override before auto setup.",
            ))
        };
    }
    if current == Some("ultra")
        && let Some(effort) = ["max", "xhigh", "high", "medium", "low", "minimal", "none"]
            .into_iter()
            .find(|effort| efforts.contains(effort))
    {
        return Ok(Some(effort.into()));
    }
    if let Some(default) = model
        .get("default_reasoning_level")
        .and_then(Value::as_str)
        .filter(|default| efforts.contains(default))
    {
        return Ok(Some(default.into()));
    }
    if current.is_some() {
        return Err(AdapterError::invalid(
            "The configured reasoning effort is unsupported and the model advertises no default.",
        ));
    }
    Ok(efforts.first().map(|effort| (*effort).into()))
}

pub(super) fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Mai => "mai",
        Provider::Github => "github",
    }
}

pub(super) fn same_upstream(left: &str, right: &str) -> bool {
    match (Url::parse(left), Url::parse(right)) {
        (Ok(left), Ok(right)) => {
            left.as_str().trim_end_matches('/') == right.as_str().trim_end_matches('/')
        }
        _ => false,
    }
}

pub(super) fn catalog_models(catalog: &Value, scope: &AccountScope) -> Result<Vec<Value>> {
    if catalog.get("provider").and_then(Value::as_str) != Some(provider_name(scope.provider))
        || (scope.provider == Provider::Github
            && !catalog
                .get("account")
                .and_then(Value::as_str)
                .is_some_and(|account| account.eq_ignore_ascii_case(&scope.account)))
    {
        return Err(AdapterError::upstream(
            "The model catalog does not match the selected provider/account.",
        ));
    }
    let models = catalog
        .get("models")
        .and_then(Value::as_array)
        .filter(|models| !models.is_empty())
        .ok_or_else(|| {
            AdapterError::upstream("The provider did not return a usable Codex model catalog.")
        })?;
    let mut seen = HashSet::new();
    for model in models {
        let id = model
            .get("slug")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| AdapterError::upstream("The provider returned an invalid model ID."))?;
        if !seen.insert(id) {
            return Err(AdapterError::upstream(
                "The provider returned duplicate model IDs.",
            ));
        }
    }
    Ok(models.clone())
}

async fn discover<S: Services>(
    services: &S,
    options: &Options,
    provider: Provider,
    context: RequestContext,
) -> Result<Ready<S::Connected>> {
    let _guard = context.cancel_on_drop();
    context
        .bounded(async {
            let (config, secret, expected) = match provider {
                Provider::Mai => (
                    BackendConfig::Mai {
                        upstream: options.upstream.clone(),
                        models_cache: models_cache(services, options)?,
                    },
                    None,
                    None,
                ),
                Provider::Github => {
                    let (credential, _) =
                        personal_credential(services, options.github_account.as_deref(), &context)
                            .await?;
                    let secret = credential.token().to_owned();
                    let expected = credential.login.clone();
                    (
                        BackendConfig::Github { credential },
                        Some(secret),
                        Some(expected),
                    )
                }
            };
            let result = async {
                let backend = services.connect(config, context.child()).await?;
                let scope = services.scope(&backend);
                if scope.provider != provider
                    || (provider == Provider::Github
                        && !expected
                            .as_ref()
                            .is_some_and(|expected| scope.account.eq_ignore_ascii_case(expected)))
                    || (provider == Provider::Mai
                        && !same_upstream(&scope.account, &options.upstream))
                {
                    return Err(AdapterError::upstream(
                        "Discovery returned a different provider, account, or upstream.",
                    ));
                }
                let catalog = services.catalog(backend.clone(), context.child()).await?;
                let models = catalog_models(&catalog, &scope)?;
                let ids = models
                    .iter()
                    .map(|model| model["slug"].as_str().unwrap().to_owned())
                    .collect();
                Ok(Ready {
                    backend,
                    catalog,
                    models,
                    available: AvailableProvider { scope, models: ids },
                })
            }
            .await;
            result.map_err(|error| {
                redact_error(
                    error,
                    &secret.iter().map(String::as_str).collect::<Vec<_>>(),
                )
            })
        })
        .await
}

pub(super) async fn select_backend<S: Services>(
    services: &S,
    options: &Options,
    cancellation: &CancellationToken,
) -> Result<Selected<S::Connected>> {
    if options.provider != ProviderMode::Auto {
        let provider = match options.provider {
            ProviderMode::Mai => Provider::Mai,
            _ => Provider::Github,
        };
        let context =
            RequestContext::with_cancellation(MANUAL_TIMEOUT, cancellation.child_token())?;
        return Ok(Selected {
            ready: discover(services, options, provider, context).await?,
            model: None,
        });
    }
    services.output(Output::Diagnostic, "Discovering existing MAI and personal GitHub providers concurrently; no login will be started.")?;
    let mai_context =
        RequestContext::with_cancellation(options.probe_timeout, cancellation.child_token())?;
    let github_context =
        RequestContext::with_cancellation(options.probe_timeout, cancellation.child_token())?;
    let (mai, github) = tokio::join!(
        discover(services, options, Provider::Mai, mai_context),
        discover(services, options, Provider::Github, github_context),
    );
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let mut ready = Vec::new();
    for (provider, result) in [(Provider::Mai, mai), (Provider::Github, github)] {
        match result {
            Ok(provider) => ready.push(provider),
            Err(error) => services.output(
                Output::Diagnostic,
                &format!(
                    "Automatic discovery: {} unavailable: {}",
                    provider_name(provider),
                    safe_text(&error.message),
                ),
            )?,
        }
    }
    let available = ready
        .iter()
        .map(|ready| ready.available.clone())
        .collect::<Vec<_>>();
    let selected = selection::select(&available, &options.preferred_models).map_err(|error| {
        if error.code != "no_preferred_model" { return error; }
        AdapterError::new(503, "no_preferred_model",
            "No preferred model is available from the existing providers. Start the MAI proxy, run login explicitly, or set --preferred-model. No unranked model or alternate account was selected.")
    })?;
    let position = ready
        .iter()
        .position(|ready| ready.available.scope == selected.scope)
        .ok_or_else(|| {
            AdapterError::upstream("The selected provider disappeared from discovery.")
        })?;
    let ready = ready.swap_remove(position);
    let model = ready
        .models
        .iter()
        .find(|model| model["slug"].as_str() == Some(&selected.model))
        .cloned()
        .ok_or_else(|| AdapterError::upstream("The selected model disappeared from discovery."))?;
    services.output(Output::Diagnostic, &services.redact(&ready.backend, &format!(
        "Automatic selection: {} via {}. Model priority precedes provider preference; MAI wins same-model ties.",
        safe_text(&selected.model), provider_name(selected.scope.provider),
    )))?;
    Ok(Selected {
        ready,
        model: Some(model),
    })
}
