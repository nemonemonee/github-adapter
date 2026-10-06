//! Foreground command composition with a stable public CLI API.

mod clients;
mod credentials;
mod discovery;
mod native;
mod options;
mod readiness;
mod services;
mod serving;

use std::ffi::OsString;

use adapter_protocol::{AdapterError, Result};
use adapter_runtime::selection::Provider;
use tokio_util::sync::CancellationToken;

use self::clients::client_command;
use self::credentials::account_command;
use self::discovery::select_backend;
use self::native::signals;
use self::serving::serve_command;

pub use self::credentials::{
    CLIENT_ID_ENV, CREDENTIAL_SERVICE, CREDENTIAL_USER, DEFAULT_CLIENT_ID, LEGACY_CLIENT_ID_ENV,
    LEGACY_TOKEN_ENV, TOKEN_ENV, decode_credential, encode_credential,
};
pub use self::discovery::compatible_reasoning_effort;
pub(crate) use self::native::read_hidden_secret;
pub use self::native::{ConnectedBackend, NativeServices};
pub use self::options::{
    Clients, Command, DEFAULT_PROBE_TIMEOUT, Launch, Options, Parsed, ProviderMode, parse_from,
};
pub use self::readiness::endpoint;
pub use self::services::{Output, Services, error_exit_code, error_text};

pub async fn dispatch<S: Services>(
    services: &S,
    mut options: Options,
    cancellation: CancellationToken,
) -> Result<i32> {
    match options.command {
        Command::Setup | Command::Restore | Command::Doctor | Command::Apps | Command::Recover => {
            client_command(services, &options, &cancellation).await
        }
        Command::Login | Command::Logout | Command::Account => {
            account_command(services, &options, &cancellation).await
        }
        Command::Serve => serve_command(services, options, cancellation).await,
        Command::Models => {
            if options.choose_provider {
                options.provider = match services.choose_provider().await? {
                    Provider::Mai => ProviderMode::Mai,
                    Provider::Github => ProviderMode::Github,
                };
            }
            let selected = select_backend(services, &options, &cancellation).await?;
            let body = serde_json::to_string_pretty(&selected.ready.catalog)
                .map_err(|_| AdapterError::upstream("Cannot encode the selected model catalog."))?;
            services.output(
                Output::Standard,
                &services.redact(&selected.ready.backend, &body),
            )?;
            Ok(0)
        }
        _ => Err(AdapterError::invalid(
            "Provider aliases must be normalized before dispatch.",
        )),
    }
}

pub async fn run_with<S, I, T>(
    services: &S,
    arguments: I,
    cancellation: CancellationToken,
) -> Result<i32>
where
    S: Services,
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match parse_from(arguments)? {
        Parsed::Display(text) => {
            services.output(Output::Standard, &text)?;
            Ok(0)
        }
        Parsed::Run(options) => dispatch(services, *options, cancellation).await,
    }
}

pub async fn run_from<I, T>(arguments: I) -> Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let services = NativeServices::default();
    match parse_from(arguments)? {
        Parsed::Display(text) => {
            services.output(Output::Standard, &text)?;
            Ok(0)
        }
        Parsed::Run(options) => {
            let cancellation = CancellationToken::new();
            let signal = signals(cancellation.clone())?;
            let result = dispatch(&services, *options, cancellation).await;
            signal.abort();
            result
        }
    }
}

pub async fn run() -> Result<i32> {
    run_from(std::env::args_os()).await
}
