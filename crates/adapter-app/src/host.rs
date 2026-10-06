//! User-owned background lifecycle. Foreground provider commands remain in `cli`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use adapter_protocol::{AdapterError, Result};
use sha2::{Digest, Sha256};

use crate::cli::{self, Command, Launch, Options, Parsed};

#[cfg(target_os = "macos")]
mod cocoa;
mod lifecycle;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(windows, target_os = "macos"))]
mod native;
#[cfg(windows)]
mod peers;
#[cfg(windows)]
mod platform;
mod protocol;
#[cfg(windows)]
mod tray;

pub use lifecycle::{Driver, HostHandle, TrayAction};
#[cfg(target_os = "macos")]
pub use macos::{
    Client, DesktopClaim, Endpoint, OwnedControl, claim_desktop, run_headless, spawn_detached,
    start_with, verify_independent_process,
};
#[cfg(windows)]
pub use peers::{PeerIdentity, PeerPolicy};
#[cfg(windows)]
pub use platform::{
    Client, DesktopClaim, Endpoint, OwnedControl, claim_desktop, run_headless, spawn_detached,
    start_with, verify_independent_process,
};
pub use protocol::{
    Activation, Control, Fault, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, Request, Response, Snapshot,
    State, decode_request, decode_response, encode_message, read_message, write_message,
};

const ENVIRONMENT: &[&str] = &[
    cli::TOKEN_ENV,
    cli::LEGACY_TOKEN_ENV,
    "GITHUB_ADAPTER_REVIEW_MODEL",
    "GITHUB_ADAPTER_RECOVER_ENCRYPTED_STATE",
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "XDG_STATE_HOME",
    "USERPROFILE",
    "HOME",
    "APPDATA",
    "LOCALAPPDATA",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

/// Values are kept in memory only; neither Debug nor serialization exposes them.
#[derive(Clone, Default)]
pub struct Inputs {
    environment: BTreeMap<String, OsString>,
    saved_credential: Option<String>,
}

impl Inputs {
    pub fn new(
        environment: impl IntoIterator<Item = (String, OsString)>,
        saved_credential: Option<String>,
    ) -> Self {
        Self {
            environment: environment
                .into_iter()
                .filter(|(name, _)| ENVIRONMENT.contains(&name.as_str()))
                .collect(),
            saved_credential,
        }
    }

    pub fn credential_stamp(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"github-adapter-credentials-v1");
        for key in [cli::TOKEN_ENV, cli::LEGACY_TOKEN_ENV] {
            hash_value(
                &mut hash,
                self.environment.get(key).map(|value| value.as_os_str()),
            );
        }
        hash_value(
            &mut hash,
            self.saved_credential.as_deref().map(std::ffi::OsStr::new),
        );
        format!("{:x}", hash.finalize())
    }
}

fn hash_value(hash: &mut Sha256, value: Option<&std::ffi::OsStr>) {
    hash.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        let bytes = value.as_encoded_bytes();
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupSpec {
    pub specification: String,
    pub credentials: String,
    pub opens_codex: bool,
}

impl StartupSpec {
    pub fn new(options: &Options, inputs: &Inputs) -> Result<Self> {
        validate_background(options)?;
        let path =
            |value: &Option<std::path::PathBuf>| value.as_deref().map(normalized_path).transpose();
        let document = serde_json::json!({
            "version": PROTOCOL_VERSION,
            "application": env!("CARGO_PKG_VERSION"),
            "provider": format!("{:?}", options.provider),
            "address": options.address.to_string(),
            "upstream": url::Url::parse(&options.upstream)
                .map_err(|_| AdapterError::invalid("Invalid background upstream."))?
                .as_str().trim_end_matches('/'),
            "models_cache": path(&options.models_cache)?,
            "github_account": options.github_account.as_ref().map(|value| value.to_ascii_lowercase()),
            "setup": options.setup,
            "launch": options.launch.map(|value| format!("{value:?}")),
            "clients": format!("{:?}", options.clients),
            "force_openai_provider": options.force_openai_provider,
            "codex_config": path(&options.codex_config)?,
            "claude_settings": path(&options.claude_settings)?,
            "backup_dir": path(&options.backup_dir)?,
            "preferred_models": options.preferred_models,
            "probe_timeout_ns": options.probe_timeout.as_nanos().to_string(),
        });
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(&document).map_err(|_| protocol::invalid_schema())?);
        // Environment-derived defaults and proxy selection are inherited once, not hot reloaded.
        for name in ENVIRONMENT {
            hash.update(name.as_bytes());
            hash_value(
                &mut hash,
                inputs.environment.get(*name).map(|value| value.as_os_str()),
            );
        }
        if [
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "XDG_STATE_HOME",
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
        ]
        .iter()
        .any(|name| {
            inputs
                .environment
                .get(*name)
                .is_some_and(|value| Path::new(value).is_relative())
        }) {
            let directory = std::env::current_dir().map_err(|_| {
                AdapterError::invalid("Cannot resolve environment-relative background paths.")
            })?;
            hash.update(normalized_path(&directory)?.as_bytes());
        }
        let credentials = inputs.credential_stamp();
        hash.update(credentials.as_bytes());
        Ok(Self {
            specification: format!("{:x}", hash.finalize()),
            credentials,
            opens_codex: options.launch == Some(Launch::Codex),
        })
    }

    pub fn request(&self) -> Request {
        Request::new(Control::Start {
            specification: self.specification.clone(),
            credentials: self.credentials.clone(),
        })
    }
}

fn normalized_path(path: &Path) -> Result<String> {
    let absolute = std::path::absolute(path)
        .map_err(|_| AdapterError::invalid("Cannot resolve a background startup path."))?;
    let text = absolute
        .to_str()
        .ok_or_else(|| AdapterError::invalid("Background startup paths must be Unicode."))?;
    #[cfg(windows)]
    {
        Ok(text.strip_prefix(r"\\?\").unwrap_or(text).to_lowercase())
    }
    #[cfg(not(windows))]
    {
        Ok(text.into())
    }
}

pub fn validate_background(options: &Options) -> Result<()> {
    if options.command != Command::Serve
        || options.choose_provider
        || options.token_login
        || options.github_client_id.is_some()
        || options.dry_run
        || options.launch == Some(Launch::Chatgpt)
    {
        return Err(AdapterError::invalid(
            "Background start supports noninteractive auto/serve/mai/github and Codex only. Run login, account, setup, restore, doctor, apps, recover, models, or interactive selection as explicit foreground commands.",
        ));
    }
    Ok(())
}

pub enum Entry {
    Foreground(Vec<OsString>),
    Start {
        arguments: Vec<OsString>,
        options: Box<Options>,
    },
    Background {
        options: Box<Options>,
        expected_specification: String,
    },
    Control(TrayAction),
    Display(String),
}

const HOST_HELP: &str = "Desktop controls:\n  github-adapter [start [auto|serve|mai|github] [options]]\n  github-adapter status\n  github-adapter open\n  github-adapter stop\n\nStart selects an available provider, connects Codex and opens it. CLI start reuses a compatible host. Quit/stop restores normal Codex settings. Stop before changing accounts or configuration.\n";
const DESKTOP_HELP: &str = "GitHub Adapter desktop app\n\nOpen with no arguments to select an available provider and connect Codex. Quit restores normal Codex settings. Serving options accept auto/serve/mai/github; use github-adapter for account, configuration and status/open/stop commands.\n";

fn help(text: String) -> String {
    let desktop = if cfg!(target_os = "macos") {
        "Open GitHub Adapter.app from Finder for the menu-bar controls."
    } else {
        "Open the GitHub Adapter desktop shortcut (github-adapter-host.exe) for the tray controls."
    };
    match text.find("Usage:") {
        Some(start) => format!(
            "{HOST_HELP}\n{desktop}\n\nForeground/serving options:\n{}",
            &text[start..]
        ),
        None => text,
    }
}

/// The internal marker is removed before the ordinary CLI parser sees any arguments.
pub fn entry_from(arguments: impl IntoIterator<Item = OsString>) -> Result<Entry> {
    let mut arguments: Vec<_> = arguments.into_iter().collect();
    if arguments.is_empty() {
        arguments.push("github-adapter".into());
    }
    let first = arguments.get(1).and_then(|value| value.to_str());
    if matches!(first, Some("status" | "open" | "stop")) {
        if arguments.len() != 2 {
            return Err(AdapterError::invalid(
                "status, open, and stop take no extra arguments.",
            ));
        }
        return Ok(Entry::Control(match first {
            Some("status") => TrayAction::Status,
            Some("open") => TrayAction::Open,
            _ => TrayAction::Quit,
        }));
    }
    let background = first == Some("--background-host");
    let start = arguments.len() == 1 || first == Some("start");
    if !start && !background {
        if arguments.iter().any(|value| value == "--background-host") {
            return Err(AdapterError::invalid(
                "The internal host marker must be first.",
            ));
        }
        if arguments
            .iter()
            .skip(1)
            .any(|value| value == "--help" || value == "-h")
        {
            let Parsed::Display(cli_help) = cli::parse_from(&arguments)? else {
                unreachable!()
            };
            return Ok(Entry::Display(help(cli_help)));
        }
        return Ok(Entry::Foreground(arguments));
    }
    if arguments.len() > 1 {
        arguments.remove(1);
    }
    let expected_specification = if background {
        let value = arguments
            .get(1)
            .and_then(|value| value.to_str())
            .and_then(|value| value.strip_prefix("--host-specification="))
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| {
                AdapterError::invalid("The internal host needs a bootstrap fingerprint. Use start.")
            })?
            .to_owned();
        arguments.remove(1);
        Some(value)
    } else {
        None
    };
    match cli::parse_from(&arguments)? {
        Parsed::Display(text) => Ok(Entry::Display(help(text))),
        Parsed::Run(options) => {
            validate_background(&options)?;
            if background {
                Ok(Entry::Background {
                    options,
                    expected_specification: expected_specification.expect("background fingerprint"),
                })
            } else {
                Ok(Entry::Start { arguments, options })
            }
        }
    }
}

pub fn parse_desktop_from(arguments: impl IntoIterator<Item = OsString>) -> Result<Parsed> {
    let mut arguments: Vec<_> = arguments.into_iter().collect();
    if arguments.is_empty() {
        arguments.push("github-adapter-host".into());
    }
    if arguments.get(1).is_some_and(|argument| argument == "start") {
        arguments.remove(1);
    }
    match cli::parse_from(arguments)? {
        Parsed::Display(text) => {
            let text = match text.find("Usage:") {
                Some(start) => format!("{DESKTOP_HELP}\nServing options:\n{}", &text[start..]),
                None => text,
            };
            Ok(Parsed::Display(text))
        }
        Parsed::Run(options) => {
            validate_background(&options)?;
            Ok(Parsed::Run(options))
        }
    }
}

/// Direct, explicitly user-activated GUI lifetime. No detached child or job-free check.
pub fn run_desktop_from(arguments: impl IntoIterator<Item = OsString>) -> Result<i32> {
    match parse_desktop_from(arguments)? {
        Parsed::Display(text) => {
            use std::io::Write;
            std::io::stdout()
                .lock()
                .write_all(text.as_bytes())
                .map_err(|_| {
                    failure(
                        "output_error",
                        "Could not write desktop help/version output.",
                    )
                })?;
            Ok(0)
        }
        Parsed::Run(options) => {
            #[cfg(windows)]
            {
                platform::run_desktop(*options)
            }
            #[cfg(target_os = "macos")]
            {
                macos::run_desktop(*options)
            }
            #[cfg(not(any(windows, target_os = "macos")))]
            {
                let _ = options;
                Err(failure(
                    "unsupported_platform",
                    "The direct desktop host requires Windows or macOS.",
                ))
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub request: Duration,
    pub attach: Duration,
    pub startup: Duration,
    pub activation: Duration,
    pub drain: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(3),
            attach: Duration::from_secs(15),
            startup: Duration::from_secs(45),
            activation: Duration::from_secs(10),
            drain: Duration::from_secs(20),
        }
    }
}

pub(crate) fn failure(code: &str, message: &str) -> AdapterError {
    AdapterError::new(500, code, message)
}

#[cfg(target_os = "macos")]
pub(crate) fn show_macos_startup_error() {
    cocoa::alert(
        "GitHub Adapter — startup needs attention",
        "Startup or Codex activation did not complete.\n\nOpen Terminal and run the bundled github-adapter status or doctor command for diagnostics.\n\nQuit — restore normal Codex stops the adapter and restores its temporary routing. Start the application from Finder to try again.",
    );
}

pub fn error_exit_code(error: &AdapterError) -> u8 {
    match error.code.as_str() {
        "tray_unavailable" => 70,
        "host_bootstrap_changed" => 71,
        "host_not_independent" => 72,
        "user_security_unavailable" => 73,
        _ => cli::error_exit_code(error),
    }
}

pub fn display_response(response: &Response) -> Result<i32> {
    println!(
        "{}",
        serde_json::to_string_pretty(response).map_err(|_| protocol::invalid_schema())?
    );
    Ok(response_exit_code(response))
}

pub fn response_exit_code(response: &Response) -> i32 {
    if response.exited && response.error.is_none() {
        0
    } else if response.error.is_some()
        || response.snapshot.state == State::Failed
        || matches!(
            response.snapshot.activation,
            Activation::Failed | Activation::Uncertain
        )
    {
        1
    } else if matches!(response.snapshot.state, State::Starting | State::Stopping)
        || matches!(
            response.snapshot.activation,
            Activation::Pending | Activation::Opening
        )
    {
        3
    } else {
        0
    }
}

pub fn run_entry(entry: Entry) -> Result<i32> {
    #[cfg(windows)]
    {
        platform::run_entry(entry)
    }
    #[cfg(target_os = "macos")]
    {
        macos::run_entry(entry)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        match entry {
            Entry::Display(text) => {
                println!("{text}");
                Ok(0)
            }
            Entry::Foreground(arguments) => foreground(arguments),
            _ => Err(failure(
                "unsupported_platform",
                "The owned tray host requires Windows.",
            )),
        }
    }
}

pub(crate) fn foreground(arguments: Vec<OsString>) -> Result<i32> {
    let runtime = runtime()?;
    let result = runtime.block_on(cli::run_from(arguments));
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| failure("runtime_error", "Could not initialize the adapter runtime."))
}
