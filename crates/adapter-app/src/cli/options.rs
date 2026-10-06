//! Argument parsing, normalized command options, and CLI defaults.

use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use adapter_protocol::{AdapterError, Result};
use adapter_runtime::selection::{self, DEFAULT_MODEL_PRIORITY};
use clap::{Parser, ValueEnum, error::ErrorKind};
use url::Url;

use crate::config::Client;

pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const MANUAL_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:5000";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Command {
    Auto,
    Serve,
    Mai,
    Github,
    Models,
    Login,
    Logout,
    Account,
    Setup,
    Restore,
    Doctor,
    Apps,
    Recover,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ProviderMode {
    Auto,
    Mai,
    Github,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Clients {
    Codex,
    Claude,
    Both,
}

impl Clients {
    pub fn selected(self) -> Vec<Client> {
        match self {
            Self::Codex => vec![Client::Codex],
            Self::Claude => vec![Client::Claude],
            Self::Both => vec![Client::Codex, Client::Claude],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Launch {
    Codex,
    Chatgpt,
}

impl Launch {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Chatgpt => "chatgpt",
        }
    }
}

#[derive(Parser)]
#[command(
    name = "github-adapter",
    version,
    about = "GitHub Adapter native command-line tools",
    long_about = "Foreground loopback serving and account/client management. Explicit auto discovers existing providers, configures Codex, then launches Codex; serve/mai/github do not configure or launch without flags. Keep foreground modes running. For the native tray host, open the GitHub Adapter desktop shortcut; status, open, and stop control its authenticated current-user instance."
)]
struct Arguments {
    #[arg(value_enum)]
    command: Option<Command>,
    #[arg(long, value_enum, conflicts_with = "choose_provider")]
    provider: Option<ProviderMode>,
    #[arg(long)]
    choose_provider: bool,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 5001)]
    port: u16,
    #[arg(long, default_value = DEFAULT_UPSTREAM)]
    upstream: String,
    #[arg(long)]
    models_cache: Option<PathBuf>,
    #[arg(long)]
    github_account: Option<String>,
    #[arg(long)]
    github_client_id: Option<String>,
    #[arg(
        long,
        help = "For login only: prompt for an OAuth token with native console echo disabled"
    )]
    token: bool,
    #[arg(long)]
    setup: bool,
    #[arg(long, value_enum)]
    launch: Option<Launch>,
    #[arg(long, value_enum)]
    client: Option<Clients>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    force_openai_provider: bool,
    #[arg(long)]
    codex_config: Option<PathBuf>,
    #[arg(long)]
    claude_settings: Option<PathBuf>,
    #[arg(long)]
    backup_dir: Option<PathBuf>,
    #[arg(long = "preferred-model", action = clap::ArgAction::Append)]
    preferred_models: Vec<String>,
    #[arg(long, value_parser = duration)]
    probe_timeout: Option<Duration>,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub command: Command,
    pub provider: ProviderMode,
    pub choose_provider: bool,
    pub address: SocketAddr,
    pub upstream: String,
    pub models_cache: Option<PathBuf>,
    pub github_account: Option<String>,
    pub github_client_id: Option<String>,
    pub token_login: bool,
    pub setup: bool,
    pub launch: Option<Launch>,
    pub clients: Clients,
    pub dry_run: bool,
    pub force_openai_provider: bool,
    pub codex_config: Option<PathBuf>,
    pub claude_settings: Option<PathBuf>,
    pub backup_dir: Option<PathBuf>,
    pub preferred_models: Vec<String>,
    pub probe_timeout: Duration,
}

#[derive(Debug)]
pub enum Parsed {
    Display(String),
    Run(Box<Options>),
}

fn duration(value: &str) -> std::result::Result<Duration, String> {
    value
        .parse::<f64>()
        .ok()
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "The timeout must be positive, finite, and representable.".into())
}

pub fn parse_from<I, T>(arguments: I) -> Result<Parsed>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let raw = match Arguments::try_parse_from(arguments) {
        Ok(raw) => raw,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            return Ok(Parsed::Display(error.to_string()));
        }
        Err(_) => {
            // Clap's normal diagnostic can echo a mistakenly supplied token value.
            return Err(AdapterError::invalid(
                "Invalid command arguments. Use --help for supported commands and options.",
            ));
        }
    };
    let command = raw
        .command
        .unwrap_or(if raw.provider.is_some() || raw.choose_provider {
            Command::Serve
        } else {
            Command::Auto
        });
    let automatic_defaults = command == Command::Auto;
    let client_command = matches!(
        command,
        Command::Setup | Command::Restore | Command::Doctor | Command::Apps | Command::Recover
    );
    if client_command && (raw.provider.is_some() || raw.upstream != DEFAULT_UPSTREAM) {
        return Err(AdapterError::invalid(
            "Client commands do not select a backend.",
        ));
    }
    if matches!(command, Command::Auto | Command::Mai | Command::Github)
        && (raw.provider.is_some() || raw.choose_provider)
    {
        return Err(AdapterError::invalid(
            "Use one provider selector, not an alias and a selector flag.",
        ));
    }
    if raw.github_client_id.is_some() && command != Command::Login {
        return Err(AdapterError::invalid(
            "--github-client-id is only valid with login.",
        ));
    }
    if raw.token && command != Command::Login {
        return Err(AdapterError::invalid("--token is only valid with login."));
    }
    if raw.choose_provider && !matches!(command, Command::Serve | Command::Models) {
        return Err(AdapterError::invalid(
            "--choose-provider is only valid with serve or models.",
        ));
    }
    let provider = match command {
        Command::Auto => ProviderMode::Auto,
        Command::Mai => ProviderMode::Mai,
        Command::Github => ProviderMode::Github,
        _ => raw.provider.unwrap_or(ProviderMode::Mai),
    };
    let command = if matches!(command, Command::Auto | Command::Mai | Command::Github) {
        Command::Serve
    } else {
        command
    };
    if provider == ProviderMode::Github && raw.upstream != DEFAULT_UPSTREAM {
        return Err(AdapterError::invalid("--upstream applies only to MAI."));
    }
    if provider != ProviderMode::Auto
        && (!raw.preferred_models.is_empty() || raw.probe_timeout.is_some())
    {
        return Err(AdapterError::invalid(
            "--preferred-model and --probe-timeout require automatic mode.",
        ));
    }
    if provider == ProviderMode::Auto && !matches!(command, Command::Serve | Command::Models) {
        return Err(AdapterError::invalid(
            "Automatic selection applies only to serve or models.",
        ));
    }
    let setup = automatic_defaults || raw.setup;
    let launch = raw.launch.or(if automatic_defaults {
        Some(Launch::Codex)
    } else {
        None
    });
    if (setup || launch.is_some()) && command != Command::Serve {
        return Err(AdapterError::invalid(
            "--setup and --launch apply only to serving.",
        ));
    }
    if raw.dry_run
        && !matches!(
            command,
            Command::Setup | Command::Restore | Command::Recover
        )
    {
        return Err(AdapterError::invalid(
            "--dry-run applies only to setup, restore, or recover.",
        ));
    }
    if raw.force_openai_provider && !(command == Command::Setup || setup) {
        return Err(AdapterError::invalid(
            "--force-openai-provider requires setup.",
        ));
    }
    let configuration = matches!(
        command,
        Command::Setup | Command::Restore | Command::Doctor | Command::Recover
    ) || setup
        || launch.is_some();
    if !configuration
        && (raw.client.is_some()
            || raw.codex_config.is_some()
            || raw.claude_settings.is_some()
            || raw.backup_dir.is_some())
    {
        return Err(AdapterError::invalid(
            "Client/path options require a client command, --setup, or --launch.",
        ));
    }
    let clients = raw.client.unwrap_or(if automatic_defaults {
        Clients::Codex
    } else {
        Clients::Both
    });
    if clients == Clients::Claude && (launch.is_some() || raw.force_openai_provider) {
        return Err(AdapterError::invalid(
            "Desktop launch and --force-openai-provider require the codex client.",
        ));
    }
    let host = raw
        .host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(&raw.host);
    let address = if host.eq_ignore_ascii_case("localhost") {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host.parse::<IpAddr>()
            .map_err(|_| AdapterError::invalid("Use a loopback IP address or localhost."))?
    };
    if !address.is_loopback() {
        return Err(AdapterError::invalid(
            "The adapter listener must be loopback-only.",
        ));
    }
    if raw.port == 0 && matches!(command, Command::Setup | Command::Doctor) {
        return Err(AdapterError::invalid(
            "setup and doctor need an existing, nonzero port.",
        ));
    }
    let upstream = Url::parse(&raw.upstream)
        .map_err(|_| AdapterError::invalid("The MAI upstream URL is invalid."))?;
    if !matches!(upstream.scheme(), "http" | "https")
        || upstream.host_str().is_none()
        || !upstream.username().is_empty()
        || upstream.password().is_some()
        || upstream.query().is_some()
        || upstream.fragment().is_some()
    {
        return Err(AdapterError::invalid(
            "Use an HTTP(S) MAI URL without credentials, query, or fragment.",
        ));
    }
    if raw
        .github_account
        .as_ref()
        .is_some_and(|account| account.trim().is_empty() || account.chars().any(char::is_control))
    {
        return Err(AdapterError::invalid(
            "--github-account must name a nonempty login.",
        ));
    }
    let preferred_models = if raw.preferred_models.is_empty() {
        DEFAULT_MODEL_PRIORITY
            .iter()
            .map(|model| (*model).into())
            .collect()
    } else {
        raw.preferred_models
    };
    selection::validate_priority(&preferred_models)?;
    if preferred_models
        .iter()
        .any(|model| model.chars().any(char::is_control))
    {
        return Err(AdapterError::invalid(
            "Preferred model IDs cannot contain control characters.",
        ));
    }
    Ok(Parsed::Run(Box::new(Options {
        command,
        provider,
        choose_provider: raw.choose_provider,
        address: SocketAddr::new(address, raw.port),
        upstream: raw.upstream,
        models_cache: raw.models_cache,
        github_account: raw.github_account,
        github_client_id: raw.github_client_id,
        token_login: raw.token,
        setup,
        launch,
        clients,
        dry_run: raw.dry_run,
        force_openai_provider: raw.force_openai_provider,
        codex_config: raw.codex_config,
        claude_settings: raw.claude_settings,
        backup_dir: raw.backup_dir,
        preferred_models,
        probe_timeout: raw.probe_timeout.unwrap_or(DEFAULT_PROBE_TIMEOUT),
    })))
}
