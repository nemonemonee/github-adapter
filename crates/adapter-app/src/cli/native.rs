//! Concrete native effects: credential storage, auth, desktop, console, and signals.

use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use adapter_protocol::{AdapterError, Result, Value};
use adapter_runtime::auth::{self, Credential};
use adapter_runtime::backend::{Backend, BackendConfig};
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::{AccountScope, Provider};
use adapter_runtime::server::{self, ServerOptions};
use futures_util::future::BoxFuture;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::credentials::{CREDENTIAL_SERVICE, CREDENTIAL_USER, redact, redact_error};
use super::options::{MANUAL_TIMEOUT, Options};
use super::services::{Output, Services, cancelled, safe_text};
use crate::config::{self, Client, ClientPaths};
use crate::desktop::{self, DesktopApp};
use crate::windows_credentials;

pub(super) async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(operation).await.map_err(|_| {
        AdapterError::new(
            500,
            "native_operation_failed",
            "A native operation could not complete.",
        )
    })?
}

type ReadyObserver = Arc<dyn Fn(SocketAddr) + Send + Sync>;

#[derive(Default)]
pub struct NativeServices {
    observer: Option<ReadyObserver>,
}

impl NativeServices {
    pub fn with_ready_observer(observer: impl Fn(SocketAddr) + Send + Sync + 'static) -> Self {
        Self {
            observer: Some(Arc::new(observer)),
        }
    }
}

#[derive(Clone)]
pub struct ConnectedBackend {
    backend: Arc<Backend>,
    secrets: Arc<Vec<String>>,
}

impl Services for NativeServices {
    type Connected = ConnectedBackend;

    fn output(&self, channel: Output, message: &str) -> Result<()> {
        match channel {
            Output::Standard => writeln!(io::stdout().lock(), "{message}"),
            Output::Diagnostic => writeln!(io::stderr().lock(), "{message}"),
        }
        .map_err(|_| AdapterError::new(500, "output_error", "Could not write CLI output."))
    }

    fn environment(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }

    fn paths(&self, options: &Options, clients: &[Client]) -> Result<ClientPaths> {
        config::default_paths(
            options.codex_config.clone(),
            options.claude_settings.clone(),
            options.backup_dir.clone(),
            clients,
        )
    }

    fn read_credential(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(blocking(|| {
            windows_credentials::read(CREDENTIAL_SERVICE, CREDENTIAL_USER)
        }))
    }

    fn write_credential(&self, value: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(blocking(move || {
            windows_credentials::write(CREDENTIAL_SERVICE, CREDENTIAL_USER, &value)
        }))
    }

    fn delete_credential(&self) -> BoxFuture<'_, Result<bool>> {
        Box::pin(blocking(|| {
            windows_credentials::delete(CREDENTIAL_SERVICE, CREDENTIAL_USER)
        }))
    }

    fn hidden_token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(blocking(|| {
            read_hidden_secret("GitHub OAuth token (input hidden): ")
        }))
    }

    fn choose_provider(&self) -> BoxFuture<'_, Result<Provider>> {
        Box::pin(blocking(|| {
            if !io::stdin().is_terminal() {
                return Err(AdapterError::invalid(
                    "Interactive selection needs a terminal. Pass --provider mai or --provider github.",
                ));
            }
            eprint!("1. MAI\n2. Personal GitHub Copilot\nProvider [1]: ");
            io::stderr()
                .flush()
                .map_err(|_| AdapterError::invalid("Could not display the provider prompt."))?;
            let mut answer = String::new();
            if io::stdin()
                .read_line(&mut answer)
                .map_err(|_| AdapterError::invalid("No provider was selected."))?
                == 0
            {
                return Err(AdapterError::invalid("No provider was selected."));
            }
            match answer.trim().to_ascii_lowercase().as_str() {
                "" | "1" | "mai" => Ok(Provider::Mai),
                "2" | "github" | "personal" => Ok(Provider::Github),
                _ => Err(AdapterError::invalid(
                    "Choose MAI or personal GitHub explicitly.",
                )),
            }
        }))
    }

    fn verify(
        &self,
        token: String,
        expected: Option<String>,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Credential>> {
        Box::pin(async move {
            auth::verify_account(&token, expected.as_deref(), &context)
                .await
                .map_err(|error| redact_error(error, &[&token]))
        })
    }

    fn device_login(
        &self,
        client_id: String,
        expected: Option<String>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Credential>> {
        Box::pin(async move {
            let begin =
                RequestContext::with_cancellation(MANUAL_TIMEOUT, cancellation.child_token())?;
            let authorization = auth::begin_device_login(&client_id, &begin).await?;
            self.output(Output::Standard, &format!("Open {}\nEnter code: {}\nChoose your personal GitHub account. Waiting for authorization...",
                safe_text(&authorization.verification_uri), safe_text(&authorization.user_code)))?;
            let polling = RequestContext::with_cancellation(
                authorization.expires_in.min(Duration::from_secs(900)),
                cancellation.child_token(),
            )?;
            auth::poll_device_login(&client_id, authorization, expected.as_deref(), &polling).await
        })
    }

    fn connect(
        &self,
        config: BackendConfig,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Self::Connected>> {
        Box::pin(async move {
            let secrets = match &config {
                BackendConfig::Github { credential } => vec![credential.token().to_owned()],
                BackendConfig::Mai { .. } => Vec::new(),
            };
            let references = secrets.iter().map(String::as_str).collect::<Vec<_>>();
            let backend = Backend::connect(config, context)
                .await
                .map_err(|error| redact_error(error, &references))?;
            let review = self
                .environment("GITHUB_ADAPTER_REVIEW_MODEL")
                .map(|value| {
                    value.into_string().map_err(|_| {
                        AdapterError::invalid("The configured review model must be UTF-8.")
                    })
                })
                .transpose()?;
            backend.set_review_model(review)?;
            let recovery_setting = self
                .environment("GITHUB_ADAPTER_RECOVER_ENCRYPTED_STATE")
                .map(|value| {
                    value.into_string().map_err(|_| {
                        AdapterError::invalid("The encrypted-state recovery setting must be UTF-8.")
                    })
                })
                .transpose()?;
            let recover = match recovery_setting.as_deref() {
                None | Some("0" | "false") => false,
                Some("1" | "true") => true,
                Some(_) => {
                    return Err(AdapterError::invalid(
                        "GITHUB_ADAPTER_RECOVER_ENCRYPTED_STATE must be 0/false or 1/true.",
                    ));
                }
            };
            backend.set_encrypted_state_recovery(recover);
            Ok(ConnectedBackend {
                backend,
                secrets: Arc::new(secrets),
            })
        })
    }

    fn scope(&self, backend: &Self::Connected) -> AccountScope {
        backend.backend.scope().clone()
    }

    fn health(&self, backend: &Self::Connected) -> Value {
        backend.backend.health()
    }

    fn redact(&self, backend: &Self::Connected, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in backend.secrets.iter() {
            text = redact(&text, &[secret]);
            if let Ok(encoded) = serde_json::to_string(secret) {
                text = redact(&text, &[&encoded[1..encoded.len() - 1]]);
            }
        }
        text
    }

    fn catalog(
        &self,
        backend: Self::Connected,
        context: RequestContext,
    ) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            backend
                .backend
                .model_catalog(&context)
                .await
                .map_err(|error| {
                    AdapterError::new(
                        error.status,
                        self.redact(&backend, &error.code),
                        self.redact(&backend, &error.message),
                    )
                })
        })
    }

    fn serve(
        &self,
        listener: TcpListener,
        backend: Self::Connected,
        options: ServerOptions,
        cancellation: CancellationToken,
    ) -> JoinHandle<Result<()>> {
        tokio::spawn(server::serve(
            listener,
            backend.backend,
            options,
            cancellation,
        ))
    }

    fn apps(&self, name: Option<String>) -> BoxFuture<'_, Result<Vec<DesktopApp>>> {
        Box::pin(blocking(move || {
            let apps = desktop::discover()?;
            match name {
                Some(name) => Ok(vec![desktop::select(&name, &apps)?]),
                None => Ok(apps),
            }
        }))
    }

    fn launch(&self, app: DesktopApp) -> BoxFuture<'_, Result<()>> {
        Box::pin(blocking(move || desktop::launch(&app)))
    }

    fn ready(&self, address: SocketAddr) -> Result<()> {
        if let Some(observer) = &self.observer {
            observer(address);
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) fn read_hidden_secret(prompt: &str) -> Result<String> {
    use std::ffi::c_void;
    use std::ptr;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
        fn ReadConsoleW(
            handle: *mut c_void,
            buffer: *mut c_void,
            count: u32,
            read: *mut u32,
            control: *mut c_void,
        ) -> i32;
        fn FlushConsoleInputBuffer(handle: *mut c_void) -> i32;
    }

    struct Mode {
        handle: *mut c_void,
        previous: u32,
        restored: bool,
    }
    impl Mode {
        fn restore(&mut self) -> Result<()> {
            // This guard owns only a temporary console-mode change, not the handle.
            if unsafe { SetConsoleMode(self.handle, self.previous) } == 0 {
                return Err(AdapterError::new(
                    500,
                    "console_mode_error",
                    "Could not restore the console input mode.",
                ));
            }
            self.restored = true;
            Ok(())
        }
    }
    impl Drop for Mode {
        fn drop(&mut self) {
            if !self.restored {
                unsafe {
                    SetConsoleMode(self.handle, self.previous);
                }
            }
        }
    }
    if !io::stdin().is_terminal() {
        return Err(AdapterError::invalid(
            "Secret entry requires an interactive Windows console; use an explicitly named environment variable for unattended access.",
        ));
    }
    // STD_INPUT_HANDLE is the documented unsigned representation of -10.
    let handle = unsafe { GetStdHandle((-10i32) as u32) };
    let mut previous = 0u32;
    if handle.is_null()
        || handle as isize == -1
        || unsafe { GetConsoleMode(handle, &mut previous) } == 0
        || unsafe { SetConsoleMode(handle, (previous | 0x0001 | 0x0002) & !0x0004) } == 0
    {
        return Err(AdapterError::invalid(
            "Could not establish hidden console input. No echoing fallback is used.",
        ));
    }
    let mut mode = Mode {
        handle,
        previous,
        restored: false,
    };
    eprint!("{prompt}");
    io::stderr()
        .flush()
        .map_err(|_| AdapterError::invalid("Could not display the hidden-input prompt."))?;
    let mut buffer = [0u16; 4096];
    let mut read = 0u32;
    // ReadConsoleW writes at most buffer.len() UTF-16 units. No token enters argv.
    let result = unsafe {
        ReadConsoleW(
            handle,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut read,
            ptr::null_mut(),
        )
    };
    eprintln!();
    if result == 0 || read == 0 {
        buffer.fill(0);
        mode.restore()?;
        return Err(cancelled());
    }
    if read as usize > buffer.len() || !matches!(buffer[read as usize - 1], 10 | 13) {
        unsafe {
            FlushConsoleInputBuffer(handle);
        }
        buffer.fill(0);
        mode.restore()?;
        return Err(AdapterError::invalid(
            "The hidden token input exceeded the console limit.",
        ));
    }
    let token = String::from_utf16(&buffer[..read as usize])
        .map(|token| token.trim_end_matches(['\r', '\n']).to_owned());
    buffer.fill(0);
    mode.restore()?;
    token.map_err(|_| AdapterError::invalid("The hidden token input was not valid text."))
}

#[cfg(target_os = "macos")]
static TERMINAL_INTERRUPTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "macos")]
pub(crate) fn read_hidden_secret(prompt: &str) -> Result<String> {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;

    if !io::stdin().is_terminal() {
        return Err(AdapterError::invalid(
            "Secret entry requires an interactive terminal; use an explicitly named environment variable for unattended access.",
        ));
    }
    struct Mode {
        fd: libc::c_int,
        previous: libc::termios,
        restored: bool,
    }
    impl Mode {
        fn restore(&mut self) -> Result<()> {
            if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.previous) } == -1 {
                return Err(AdapterError::new(
                    500,
                    "console_mode_error",
                    "Could not restore the terminal input mode.",
                ));
            }
            self.restored = true;
            Ok(())
        }
    }
    impl Drop for Mode {
        fn drop(&mut self) {
            if !self.restored {
                unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.previous) };
            }
        }
    }
    let fd = io::stdin().as_raw_fd();
    let mut previous: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut previous) } == -1 {
        return Err(AdapterError::invalid(
            "Could not inspect terminal input mode.",
        ));
    }
    let mut hidden = previous;
    hidden.c_lflag &= !(libc::ECHO | libc::ECHONL);
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } == -1 {
        return Err(AdapterError::invalid(
            "Could not establish hidden terminal input. No echoing fallback is used.",
        ));
    }
    let mut mode = Mode {
        fd,
        previous,
        restored: false,
    };
    eprint!("{prompt}");
    io::stderr()
        .flush()
        .map_err(|_| AdapterError::invalid("Could not display the hidden-input prompt."))?;
    let mut bytes = [0u8; 4096];
    let result = loop {
        if TERMINAL_INTERRUPTED.load(Ordering::Acquire) {
            unsafe { libc::tcflush(fd, libc::TCIFLUSH) };
            break Err(cancelled());
        }
        // A bounded poll allows the registered SIGINT/SIGTERM task to cancel
        // hidden input and restore echo, without leaving an orphaned blocking read.
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
        if ready == 0 {
            continue;
        }
        if ready == -1 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break Err(AdapterError::invalid("Hidden terminal input failed."));
        }
        let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if read <= 0 {
            break Err(cancelled());
        }
        let read = read as usize;
        if !matches!(bytes[read - 1], b'\r' | b'\n') {
            unsafe { libc::tcflush(fd, libc::TCIFLUSH) };
            break Err(AdapterError::invalid(
                "The hidden token input exceeded the terminal limit.",
            ));
        }
        break std::str::from_utf8(&bytes[..read])
            .map(|value| value.trim_end_matches(['\r', '\n']).to_owned())
            .map_err(|_| AdapterError::invalid("The hidden token input was not valid text."));
    };
    for byte in &mut bytes {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
    eprintln!();
    mode.restore()?;
    result
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn read_hidden_secret(_: &str) -> Result<String> {
    Err(AdapterError::invalid(
        "Native hidden token entry requires a Windows console. Use GITHUB_ADAPTER_TOKEN explicitly for unattended access.",
    ))
}

pub(super) fn signals(cancellation: CancellationToken) -> Result<JoinHandle<()>> {
    #[cfg(windows)]
    {
        // Register synchronously before any hidden console read starts.
        let mut signal = tokio::signal::windows::ctrl_c().map_err(|_| {
            AdapterError::new(
                500,
                "signal_error",
                "Could not register foreground Ctrl+C handling.",
            )
        })?;
        Ok(tokio::spawn(async move {
            signal.recv().await;
            cancellation.cancel();
        }))
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let register = |kind, message| {
            signal(kind).map_err(|_| AdapterError::new(500, "signal_error", message))
        };
        let mut interrupt = register(
            SignalKind::interrupt(),
            "Could not register foreground Ctrl+C handling.",
        )?;
        let mut terminate = register(
            SignalKind::terminate(),
            "Could not register foreground termination handling.",
        )?;
        #[cfg(target_os = "macos")]
        TERMINAL_INTERRUPTED.store(false, std::sync::atomic::Ordering::Release);
        Ok(tokio::spawn(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
            #[cfg(target_os = "macos")]
            TERMINAL_INTERRUPTED.store(true, std::sync::atomic::Ordering::Release);
            cancellation.cancel();
        }))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = cancellation;
        Err(AdapterError::new(
            500,
            "signal_error",
            "Foreground signals are unsupported on this platform.",
        ))
    }
}
