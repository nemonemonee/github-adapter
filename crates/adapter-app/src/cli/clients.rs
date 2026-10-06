//! Selected-client workflows and guarded frontend configuration.

use std::time::Duration;

use adapter_protocol::{AdapterError, Result, Value};
use adapter_runtime::context::RequestContext;
use tokio_util::sync::CancellationToken;

use super::discovery::{Selected, compatible_reasoning_effort};
use super::native::blocking;
use super::options::{Command, Options};
use super::readiness::{HEALTH_BYTES, READINESS_TIMEOUT, endpoint, local_json, recognized_health};
use super::services::{Output, Services, safe_text};
use crate::config::{
    self, Client, ClientPaths, CodexSettings, ConfigurationChange, ConfigureOptions,
};

fn client_name(client: Client) -> &'static str {
    match client {
        Client::Codex => "codex",
        Client::Claude => "claude",
    }
}

fn changes<S: Services>(
    services: &S,
    changes: &[ConfigurationChange],
    dry_run: bool,
) -> Result<()> {
    for change in changes {
        services.output(
            Output::Standard,
            &format!(
                "{}: {}{} - {}",
                client_name(change.client),
                if dry_run && change.changed {
                    "Would "
                } else {
                    ""
                },
                safe_text(&change.action),
                safe_text(&change.path.display().to_string())
            ),
        )?;
    }
    if !dry_run && changes.iter().any(|change| change.changed) {
        services.output(
            Output::Standard,
            "Restart the affected client to reload settings. Credentials were not changed.",
        )?;
    }
    Ok(())
}

pub(super) async fn client_command<S: Services>(
    services: &S,
    options: &Options,
    cancellation: &CancellationToken,
) -> Result<i32> {
    if options.command == Command::Apps {
        let context =
            RequestContext::with_cancellation(READINESS_TIMEOUT, cancellation.child_token())?;
        let apps = context.bounded(services.apps(None)).await?;
        if apps.is_empty() {
            services.output(
                Output::Standard,
                "No supported Codex or ChatGPT desktop apps were found; nothing was installed.",
            )?;
        }
        for app in apps {
            services.output(
                Output::Standard,
                &format!("{}\t{}", safe_text(&app.name), safe_text(&app.target)),
            )?;
        }
        return Ok(0);
    }
    let clients = options.clients.selected();
    let paths = services.paths(options, &clients)?;
    let endpoint = endpoint(options.address);
    match options.command {
        Command::Setup => {
            if clients.contains(&Client::Codex) {
                let read_paths = paths.clone();
                let settings = blocking(move || config::read_codex_settings(&read_paths)).await?;
                frontend_guards(&settings, None, options.force_openai_provider)?;
            }
            let request = ConfigureOptions {
                dry_run: options.dry_run,
                force_openai_provider: options.force_openai_provider,
                ..Default::default()
            };
            let result =
                blocking(move || config::configure(&paths, &endpoint, &clients, &request)).await?;
            changes(services, &result, options.dry_run)?;
            Ok(0)
        }
        Command::Restore => {
            let dry_run = options.dry_run;
            let result = blocking(move || config::restore(&paths, &clients, dry_run)).await?;
            changes(services, &result, dry_run)?;
            Ok(0)
        }
        Command::Recover => {
            let dry_run = options.dry_run;
            let paths = ClientPaths {
                codex_config: if clients.contains(&Client::Codex) {
                    paths.codex_config
                } else {
                    None
                },
                claude_settings: if clients.contains(&Client::Claude) {
                    paths.claude_settings
                } else {
                    None
                },
                backup_dir: paths.backup_dir,
            };
            let result = blocking(move || config::recover(&paths, dry_run)).await?;
            changes(services, &result, dry_run)?;
            if result.is_empty() {
                services.output(
                    Output::Standard,
                    "No pending native configuration transaction.",
                )?;
            }
            Ok(0)
        }
        Command::Doctor => {
            let inspect_paths = paths.clone();
            let inspect_clients = clients.clone();
            let statuses = blocking(move || {
                config::inspect(&inspect_paths, &endpoint, &inspect_clients, None, None)
            })
            .await?;
            let configured = statuses
                .iter()
                .all(|status| status.configured && status.issues.is_empty());
            for status in statuses {
                services.output(
                    Output::Standard,
                    &format!(
                        "{}: endpoint {}; managed {}; automatic restore {}",
                        client_name(status.client),
                        if status.configured {
                            "matches"
                        } else {
                            "does not match"
                        },
                        if status.managed { "yes" } else { "no" },
                        if status.restorable {
                            "ready"
                        } else {
                            "not available"
                        },
                    ),
                )?;
                for issue in status.issues {
                    services.output(Output::Standard, &format!("  {}", safe_text(&issue)))?;
                }
            }
            if clients.contains(&Client::Codex) {
                let settings = blocking(move || config::read_codex_settings(&paths)).await?;
                services.output(Output::Standard, &format!(
                    "Codex effective overrides: custom provider {}; openai provider table {}; active profile fields [{}]; model {}; reasoning {}; context {:?}; compaction {:?}.",
                    if settings.provider == "openai" { "no" } else { "yes" },
                    if settings.openai_provider_override { "yes" } else { "no" },
                    settings.profile_overrides.iter().map(|field| safe_text(field)).collect::<Vec<_>>().join(", "),
                    if settings.model.is_some() { "set" } else { "unset" },
                    if settings.reasoning_effort.is_some() { "set" } else { "unset" },
                    settings.context_window, settings.auto_compact_token_limit,
                ))?;
            }
            let context = RequestContext::with_cancellation(
                Duration::from_secs(2),
                cancellation.child_token(),
            )?;
            match local_json(options.address, "/health", HEALTH_BYTES, &context)
                .await
                .and_then(recognized_health)
            {
                Ok(health) => {
                    services.output(Output::Standard, &format!("Local health: GitHub Adapter responds via {}. This is not an inference check.", health["provider"].as_str().unwrap()))?;
                    Ok(if configured { 0 } else { 1 })
                }
                Err(error) => {
                    services.output(Output::Standard, &format!("Local health unavailable: {}. This does not establish that the port is free.", safe_text(&error.message)))?;
                    Ok(1)
                }
            }
        }
        _ => Err(AdapterError::invalid("Not a client command.")),
    }
}

fn frontend_guards(settings: &CodexSettings, model: Option<&Value>, force: bool) -> Result<()> {
    if settings.openai_provider_override || !settings.profile_overrides.is_empty() {
        return Err(AdapterError::invalid(
            "An active Codex profile or model_providers.openai table overrides the selected endpoint/model. Resolve it explicitly; its contents were preserved.",
        ));
    }
    if settings.provider != "openai" && !force {
        return Err(AdapterError::invalid(
            "Codex uses a custom provider. Use explicit --force-openai-provider with setup; no provider was forced automatically.",
        ));
    }
    if let Some(model) = model {
        let context = model.get("context_window").and_then(Value::as_u64);
        if [settings.context_window, settings.auto_compact_token_limit]
            .into_iter()
            .flatten()
            .any(|limit| context.is_none_or(|context| limit > context))
        {
            return Err(AdapterError::invalid(
                "A Codex context/compaction override exceeds the selected model's known context. Resolve it explicitly; it was not changed.",
            ));
        }
    }
    Ok(())
}

pub(super) async fn prepare_frontend<S: Services>(
    services: &S,
    options: &Options,
    selected: &Selected<S::Connected>,
    endpoint: &str,
) -> Result<Option<String>> {
    if !options.setup && options.launch.is_none() {
        return Ok(None);
    }
    let clients = options.clients.selected();
    let paths = services.paths(options, &clients)?;
    let mut configure = ConfigureOptions {
        force_openai_provider: options.force_openai_provider,
        ..Default::default()
    };
    let mut expected_model = None;
    if clients.contains(&Client::Codex) {
        let read_paths = paths.clone();
        let settings = blocking(move || config::read_codex_settings(&read_paths)).await?;
        frontend_guards(
            &settings,
            selected.model.as_ref(),
            options.force_openai_provider,
        )?;
        if let Some(model) = &selected.model {
            let id = model["slug"]
                .as_str()
                .ok_or_else(|| AdapterError::upstream("The selected model has no ID."))?;
            configure.model = Some(id.into());
            configure.reasoning_effort =
                compatible_reasoning_effort(model, settings.reasoning_effort.as_deref())?;
            if services.redact(&selected.ready.backend, id) != id
                || configure.reasoning_effort.as_ref().is_some_and(|effort| {
                    services.redact(&selected.ready.backend, effort) != *effort
                })
            {
                return Err(AdapterError::upstream(
                    "The selected model metadata contains credential material.",
                ));
            }
            expected_model = configure.model.clone();
        } else if options.launch.is_some() {
            expected_model = settings.model.clone();
            if let Some(id) = &expected_model {
                let model = selected.ready.models.iter().find(|model| model["slug"].as_str() == Some(id))
                    .ok_or_else(|| AdapterError::invalid("The configured Codex model is not advertised by this backend. Use auto or select an advertised model."))?;
                if compatible_reasoning_effort(model, settings.reasoning_effort.as_deref())?
                    .as_deref()
                    != settings.reasoning_effort.as_deref()
                    && settings.reasoning_effort.is_some()
                {
                    return Err(AdapterError::invalid(
                        "The configured Codex reasoning effort is unsupported. Use auto or fix it explicitly.",
                    ));
                }
            }
        }
    }
    if options.setup {
        let write_paths = paths.clone();
        let write_clients = clients.clone();
        let write_endpoint = endpoint.to_owned();
        let write_options = configure.clone();
        let result = services
            .configure_launch(write_paths, write_clients, write_endpoint, write_options)
            .await?;
        changes(services, &result, false)?;
    }
    let endpoint = endpoint.to_owned();
    let statuses = blocking(move || {
        config::inspect(
            &paths,
            &endpoint,
            &clients,
            configure.model.as_deref(),
            configure.reasoning_effort.as_deref(),
        )
    })
    .await?;
    if !statuses
        .iter()
        .filter(|status| options.setup || status.client == Client::Codex)
        .all(|status| status.configured)
    {
        return Err(AdapterError::invalid(
            "Selected client settings do not match this adapter/model. Use --setup; no desktop app was launched.",
        ));
    }
    for status in statuses {
        for issue in status.issues {
            services.output(
                Output::Diagnostic,
                &format!(
                    "Configuration backup warning ({}): {}",
                    client_name(status.client),
                    safe_text(&issue)
                ),
            )?;
        }
    }
    Ok(expected_model)
}
