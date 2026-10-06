//! Adapter-only environment selection, credential format, redaction, and account commands.

use adapter_protocol::{AdapterError, Result, Value, json};
use adapter_runtime::auth::Credential;
use adapter_runtime::backend::BackendConfig;
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::Provider;
use serde_json::json as value;
use tokio_util::sync::CancellationToken;

use super::options::{Command, MANUAL_TIMEOUT, Options};
use super::services::{Output, Services, cancelled, safe_text};

pub const CREDENTIAL_SERVICE: &str = "MAI Adapter";
pub const CREDENTIAL_USER: &str = "personal-github";
pub const TOKEN_ENV: &str = "GITHUB_ADAPTER_TOKEN";
pub const LEGACY_TOKEN_ENV: &str = "MAI_ADAPTER_GITHUB_TOKEN";
pub const CLIENT_ID_ENV: &str = "GITHUB_ADAPTER_CLIENT_ID";
pub const LEGACY_CLIENT_ID_ENV: &str = "MAI_ADAPTER_GITHUB_CLIENT_ID";
pub const DEFAULT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

pub(super) fn redact(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_owned(), |text, secret| {
            text.replace(secret, "[redacted]")
        })
}

pub(super) fn redact_error(error: AdapterError, secrets: &[&str]) -> AdapterError {
    AdapterError::new(
        error.status,
        redact(&error.code, secrets),
        redact(&error.message, secrets),
    )
}

fn override_value<S: Services>(
    services: &S,
    primary: &'static str,
    legacy: &'static str,
) -> Result<Option<(&'static str, String)>> {
    let first = services.environment(primary);
    let second = services.environment(legacy);
    if first.is_some() && second.is_some() && first != second {
        return Err(AdapterError::new(
            401,
            "credential_conflict",
            format!(
                "{primary} and {legacy} conflict. Unset one explicitly; no personal account was selected."
            ),
        ));
    }
    match first
        .map(|value| (primary, value))
        .or_else(|| second.map(|value| (legacy, value)))
    {
        Some((name, value)) => value
            .into_string()
            .map(|value| Some((name, value)))
            .map_err(|_| {
                AdapterError::new(
                    401,
                    "invalid_credential",
                    format!("{name} must contain Unicode text."),
                )
            }),
        None => Ok(None),
    }
}

pub fn decode_credential(value: &str) -> Result<Credential> {
    let invalid = || {
        AdapterError::new(
            401,
            "invalid_credential",
            "The saved personal GitHub credential is invalid; run login again.",
        )
    };
    let value = json::decode(value.as_bytes(), 16 * 1024).map_err(|_| invalid())?;
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(invalid());
    }
    let login = value
        .get("login")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    Credential::new(login.into(), token.into()).map_err(|_| invalid())
}

pub fn encode_credential(credential: &Credential) -> Result<String> {
    serde_json::to_string(&value!({
        "version": 1, "login": credential.login, "token": credential.token()
    }))
    .map_err(|_| {
        AdapterError::new(
            500,
            "credential_encoding",
            "Could not encode the verified credential.",
        )
    })
}

fn expected_login(credential: &Credential, expected: Option<&str>) -> Result<()> {
    if expected.is_some_and(|expected| !credential.login.eq_ignore_ascii_case(expected)) {
        return Err(AdapterError::new(
            401,
            "account_mismatch",
            "The personal GitHub credential belongs to a different login. No other account was selected.",
        ));
    }
    Ok(())
}

pub(super) async fn personal_credential<S: Services>(
    services: &S,
    expected: Option<&str>,
    context: &RequestContext,
) -> Result<(Credential, Option<&'static str>)> {
    if let Some((name, token)) = override_value(services, TOKEN_ENV, LEGACY_TOKEN_ENV)? {
        let credential = context
            .bounded(services.verify(token.clone(), expected.map(str::to_owned), context.child()))
            .await
            .map_err(|error| redact_error(error, &[&token]))?;
        expected_login(&credential, expected)?;
        return Ok((credential, Some(name)));
    }
    let value = context
        .bounded(services.read_credential())
        .await?
        .ok_or_else(|| {
            AdapterError::new(
                401,
                "missing_credential",
                "No personal GitHub account is saved. Run login explicitly; no login was started.",
            )
        })?;
    let credential = decode_credential(&value)?;
    expected_login(&credential, expected)?;
    Ok((credential, None))
}

fn warn_token_overrides<S: Services>(services: &S) -> Result<()> {
    for name in [TOKEN_ENV, LEGACY_TOKEN_ENV] {
        if services.environment(name).is_some() {
            services.output(
                Output::Standard,
                &format!(
                    "{name} is still set; unset it separately to use the saved-account state."
                ),
            )?;
        }
    }
    Ok(())
}

pub(super) async fn account_command<S: Services>(
    services: &S,
    options: &Options,
    cancellation: &CancellationToken,
) -> Result<i32> {
    match options.command {
        Command::Logout => {
            let removed = services.delete_credential().await?;
            services.output(
                Output::Standard,
                if removed {
                    "Removed this adapter's saved personal GitHub credential."
                } else {
                    "This adapter has no saved personal GitHub credential."
                },
            )?;
            warn_token_overrides(services)?;
            services.output(Output::Standard, "Running adapters retain in-memory tokens. No running process was stopped and GitHub authorization was not revoked.")?;
            Ok(0)
        }
        Command::Account => {
            let context =
                RequestContext::with_cancellation(MANUAL_TIMEOUT, cancellation.child_token())?;
            match personal_credential(services, options.github_account.as_deref(), &context).await {
                Ok((credential, source)) => {
                    services.output(
                        Output::Standard,
                        &format!(
                            "{} personal GitHub account: {}{}",
                            if source.is_some() {
                                "Verified"
                            } else {
                                "Saved"
                            },
                            safe_text(&redact(&credential.login, &[credential.token()])),
                            source
                                .map(|source| format!(" ({source})"))
                                .unwrap_or_default(),
                        ),
                    )?;
                    Ok(0)
                }
                Err(error) if error.code == "missing_credential" => {
                    services.output(
                        Output::Standard,
                        "No personal GitHub account is saved. Run login.",
                    )?;
                    Ok(1)
                }
                Err(error) => Err(error),
            }
        }
        Command::Login => {
            let client_id = match &options.github_client_id {
                Some(value) => value.clone(),
                None => override_value(services, CLIENT_ID_ENV, LEGACY_CLIENT_ID_ENV)?
                    .map(|(_, value)| value)
                    .unwrap_or_else(|| DEFAULT_CLIENT_ID.into()),
            };
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            let credential = if options.token_login {
                let token = services.hidden_token().await?;
                let context =
                    RequestContext::with_cancellation(MANUAL_TIMEOUT, cancellation.child_token())?;
                services
                    .verify(token.clone(), options.github_account.clone(), context)
                    .await
                    .map_err(|error| redact_error(error, &[&token]))?
            } else {
                if client_id == DEFAULT_CLIENT_ID {
                    services.output(
                        Output::Standard,
                        "Device sign-in uses the public GitHub Copilot OAuth client.",
                    )?;
                }
                services
                    .device_login(
                        client_id,
                        options.github_account.clone(),
                        cancellation.child_token(),
                    )
                    .await?
            };
            expected_login(&credential, options.github_account.as_deref())?;
            let context =
                RequestContext::with_cancellation(MANUAL_TIMEOUT, cancellation.child_token())?;
            let backend = context
                .bounded(services.connect(
                    BackendConfig::Github {
                        credential: credential.clone(),
                    },
                    context.child(),
                ))
                .await
                .map_err(|error| redact_error(error, &[credential.token()]))?;
            let scope = services.scope(&backend);
            if scope.provider != Provider::Github
                || !scope.account.eq_ignore_ascii_case(&credential.login)
            {
                return Err(AdapterError::upstream(
                    "Backend readiness returned a different account. Nothing was saved.",
                ));
            }
            context.check()?;
            services
                .write_credential(encode_credential(&credential)?)
                .await
                .map_err(|error| redact_error(error, &[credential.token()]))?;
            services.output(
                Output::Standard,
                &format!(
                    "Signed in as {}. Credential saved in the system credential store.",
                    safe_text(&redact(&credential.login, &[credential.token()]))
                ),
            )?;
            services.output(
                Output::Standard,
                "Restart running personal adapters to use this account.",
            )?;
            warn_token_overrides(services)?;
            Ok(0)
        }
        _ => Err(AdapterError::invalid("Not an account command.")),
    }
}
