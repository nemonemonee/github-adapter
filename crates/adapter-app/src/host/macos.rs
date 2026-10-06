//! Same-user Unix control and native menu-bar lifetime. No service or shell daemon.
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adapter_protocol::{AdapterError, Result};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{
    Activation, Control, Driver, Entry, Fault, HostHandle, Limits, Request, Response, StartupSpec,
    State, TrayAction, cocoa, decode_request, decode_response, display_response, failure,
    lifecycle, native, read_message, write_message,
};

fn untrusted() -> AdapterError {
    AdapterError::new(
        409,
        "host_owner_mismatch",
        "The control socket's owner or peer executable could not be verified. No control request was sent or process stopped.",
    )
}

fn secure_directory(path: &Path) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(untrusted()),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| untrusted())?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(untrusted());
    }
    Ok(())
}

fn peer_images(current: PathBuf) -> Result<Vec<PathBuf>> {
    let counterpart = match current.file_name().and_then(|name| name.to_str()) {
        Some("github-adapter") => Some("github-adapter-host"),
        Some("github-adapter-host") => Some("github-adapter"),
        _ => None,
    };
    let mut images = vec![current.clone()];
    if let Some(name) = counterpart {
        let sibling = current.parent().ok_or_else(untrusted)?.join(name);
        if let Ok(metadata) = std::fs::symlink_metadata(&sibling)
            && metadata.is_file()
            && !metadata.file_type().is_symlink()
        {
            let image = std::fs::canonicalize(&sibling).map_err(|_| untrusted())?;
            if image == sibling {
                images.push(image);
            }
        }
    }
    Ok(images)
}

fn process_image(pid: u32) -> Result<PathBuf> {
    let mut image = vec![0u8; 4096];
    let length = unsafe {
        libc::proc_pidpath(
            pid as libc::pid_t,
            image.as_mut_ptr().cast(),
            image.len() as u32,
        )
    };
    if length <= 0 {
        return Err(untrusted());
    }
    let end = image.iter().position(|b| *b == 0).ok_or_else(untrusted)?;
    use std::os::unix::ffi::OsStrExt;
    std::fs::canonicalize(Path::new(std::ffi::OsStr::from_bytes(&image[..end])))
        .map_err(|_| untrusted())
}

#[derive(Clone)]
pub struct Endpoint {
    socket: PathBuf,
    lock: PathBuf,
    images: Vec<PathBuf>,
}

impl Endpoint {
    pub fn current() -> Result<Self> {
        Self::isolated("desktop")
    }

    pub fn isolated(namespace: &str) -> Result<Self> {
        if namespace.is_empty()
            || namespace.len() > 32
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(AdapterError::invalid("Invalid host namespace."));
        }
        // /private/tmp is system-owned; no environment variable chooses the IPC root.
        let directory = PathBuf::from(format!("/private/tmp/github-adapter-{}", unsafe {
            libc::geteuid()
        }));
        secure_directory(&directory)?;
        let current = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|_| untrusted())?;
        let images = peer_images(current)?;
        Ok(Self {
            socket: directory.join(format!("{namespace}.sock")),
            lock: directory.join(format!("{namespace}.lock")),
            images,
        })
    }

    pub fn pipe_name(&self) -> &Path {
        &self.socket
    }

    fn authenticate(&self, stream: &UnixStream) -> Result<Peer> {
        let mut uid = 0;
        let mut gid = 0;
        let mut pid: libc::pid_t = 0;
        let mut size = std::mem::size_of_val(&pid) as libc::socklen_t;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0
            || uid != unsafe { libc::geteuid() }
            || unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_LOCAL,
                    libc::LOCAL_PEERPID,
                    (&mut pid as *mut libc::pid_t).cast(),
                    &mut size,
                )
            } != 0
            || size as usize != std::mem::size_of_val(&pid)
            || pid <= 0
        {
            return Err(untrusted());
        }
        let created = crate::on_demand::process_created(pid as u32)?.ok_or_else(untrusted)?;
        let path = process_image(pid as u32)?;
        if !self.images.contains(&path) {
            return Err(untrusted());
        }
        Ok(Peer {
            pid: pid as u32,
            created,
            image: path,
        })
    }
}

struct Peer {
    pid: u32,
    created: u64,
    image: PathBuf,
}
impl Peer {
    fn alive(&self) -> Result<bool> {
        if crate::on_demand::process_created(self.pid)? != Some(self.created) {
            return Ok(false);
        }
        // exec(2) preserves PID and creation time. Recheck the image before dispatch.
        if process_image(self.pid)? != self.image {
            return Err(untrusted());
        }
        Ok(true)
    }
}

pub struct OwnedControl {
    endpoint: Endpoint,
    listener: UnixListener,
    inode: u64,
}
impl OwnedControl {
    pub fn bind(endpoint: Endpoint) -> Result<Self> {
        let listener = UnixListener::bind(&endpoint.socket).map_err(|_|
            failure("host_instance_unavailable", "Cannot claim the current-user control socket. Resolve the existing instance explicitly."))?;
        std::fs::set_permissions(&endpoint.socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| untrusted())?;
        let inode = std::fs::symlink_metadata(&endpoint.socket)
            .map_err(|_| untrusted())?
            .ino();
        Ok(Self {
            endpoint,
            listener,
            inode,
        })
    }

    async fn serve(
        &self,
        host: HostHandle,
        limits: Limits,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                accepted = self.listener.accept(), if connections.len() < 24 => {
                    let (mut stream, _) = accepted.map_err(|_| failure("host_control_failed", "The owned control listener failed."))?;
                    let Ok(peer) = self.endpoint.authenticate(&stream) else { continue; };
                    let host = host.clone();
                    connections.spawn(async move {
                        let _ = tokio::time::timeout(limits.request, async {
                            let bytes = read_message(&mut stream).await?;
                            if !peer.alive()? { return Err(untrusted()); }
                            let response = match decode_request(&bytes) {
                                Ok(request) => host.request(&request),
                                Err(error) => host.response(Some(Fault::new(&error.code, &error.message))),
                            };
                            write_message(&mut stream, &response).await
                        }).await;
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        let _ = tokio::time::timeout(limits.request, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }
}
impl Drop for OwnedControl {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.endpoint.socket).is_ok_and(|m| m.ino() == self.inode) {
            let _ = std::fs::remove_file(&self.endpoint.socket);
        }
    }
}

#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    limits: Limits,
}
impl Client {
    pub fn new(endpoint: Endpoint, limits: Limits) -> Self {
        Self { endpoint, limits }
    }
    async fn exchange(&self, request: &Request) -> Result<(Response, Peer)> {
        request.validate()?;
        tokio::time::timeout(self.limits.request, async {
            if let Ok(metadata) = std::fs::symlink_metadata(&self.endpoint.socket) {
                use std::os::unix::fs::FileTypeExt;
                if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o077 != 0 { return Err(untrusted()); }
            }
            let mut stream = UnixStream::connect(&self.endpoint.socket).await.map_err(|error| {
                if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) {
                    AdapterError::new(404, "host_not_running", "No owned GitHub Adapter host is running.")
                } else { untrusted() }
            })?;
            let peer = self.endpoint.authenticate(&stream)?;
            write_message(&mut stream, request).await?;
            let response = decode_response(&read_message(&mut stream).await?)?;
            // Stop may drain and exit immediately after writing the acknowledgment.
            // An alive peer must still be the authenticated executable after the read.
            let _ = peer.alive()?;
            if response.process_id != peer.pid { return Err(untrusted()); }
            Ok((response, peer))
        }).await.map_err(|_| AdapterError::new(504, "host_control_timeout", "The bounded control request timed out. No replacement or forced stop was attempted."))?
    }
    pub async fn request(&self, request: &Request) -> Result<Response> {
        self.exchange(request).await.map(|r| r.0)
    }
    pub async fn status(&self) -> Result<Response> {
        self.request(&Request::new(Control::Status)).await
    }
    pub async fn wait_result(&self, mut response: Response, budget: Duration) -> Result<Response> {
        let deadline = tokio::time::Instant::now() + budget;
        let pid = response.process_id;
        loop {
            if response.error.is_some()
                || matches!(response.snapshot.state, State::Failed | State::Stopping)
                || (response.snapshot.state == State::Ready
                    && !matches!(
                        response.snapshot.activation,
                        Activation::Pending | Activation::Opening
                    ))
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(response);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let next = match tokio::time::timeout_at(deadline, self.status()).await {
                Ok(result) => result?,
                Err(_) => return Ok(response),
            };
            if next.process_id != pid {
                return Err(failure(
                    "host_instance_changed",
                    "The host changed while waiting; no activation was retried.",
                ));
            }
            response = next;
        }
    }
    pub async fn stop_and_wait(&self) -> Result<Response> {
        let (mut response, peer) = self.exchange(&Request::new(Control::Stop)).await?;
        if response.error.is_some() {
            return Ok(response);
        }
        let deadline =
            Instant::now() + self.limits.drain + self.limits.request + Duration::from_secs(3);
        while peer.alive()? {
            if Instant::now() >= deadline {
                return Err(failure(
                    "host_stop_timeout",
                    "The authenticated host has not exited. No process was forcibly terminated.",
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        response.exited = true;
        Ok(response)
    }
}

fn start_guard(endpoint: &Endpoint, budget: Duration) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&endpoint.lock)
        .map_err(|_| untrusted())?;
    let metadata = file.metadata().map_err(|_| untrusted())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(untrusted());
    }
    let deadline = Instant::now() + budget;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EWOULDBLOCK)
            || Instant::now() >= deadline
        {
            return Err(failure(
                "host_starting",
                "Another launcher owns the startup transaction. No extra host was spawned.",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// A refused connection establishes there is no live listener. Only an owned plain socket
// is removed under the launcher lock; symlinks and unrelated filesystem entries survive.
fn clear_stale(endpoint: &Endpoint) -> Result<()> {
    match std::fs::symlink_metadata(&endpoint.socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(untrusted());
            }
            std::fs::remove_file(&endpoint.socket).map_err(|_| untrusted())
        }
        Err(_) => Err(untrusted()),
    }
}
pub enum DesktopClaim {
    Owned(OwnedControl),
    Reused(Response),
}
pub fn claim_desktop(
    runtime: &tokio::runtime::Runtime,
    endpoint: Endpoint,
    specification: &StartupSpec,
    limits: Limits,
) -> Result<DesktopClaim> {
    let client = Client::new(endpoint.clone(), limits);
    match runtime.block_on(client.request(&specification.request())) {
        Ok(response) => {
            return Ok(DesktopClaim::Reused(
                runtime.block_on(client.wait_result(response, limits.attach))?,
            ));
        }
        Err(error) if error.code == "host_not_running" => {}
        Err(error) => return Err(error),
    }
    let _guard = start_guard(&endpoint, limits.request)?;
    match runtime.block_on(client.request(&specification.request())) {
        Ok(response) => Ok(DesktopClaim::Reused(
            runtime.block_on(client.wait_result(response, limits.attach))?,
        )),
        Err(error) if error.code == "host_not_running" => {
            clear_stale(&endpoint)?;
            let _entered = runtime.enter();
            Ok(DesktopClaim::Owned(OwnedControl::bind(endpoint)?))
        }
        Err(error) => Err(error),
    }
}
pub fn start_with(
    endpoint: Endpoint,
    specification: &StartupSpec,
    limits: Limits,
    spawn: impl FnOnce() -> Result<Option<Child>>,
) -> Result<Response> {
    let runtime = super::runtime()?;
    let result = (|| {
        let client = Client::new(endpoint.clone(), limits);
        let request = specification.request();
        match runtime.block_on(client.request(&request)) {
            Ok(response) => return runtime.block_on(client.wait_result(response, limits.attach)),
            Err(error) if error.code == "host_not_running" => {}
            Err(error) => return Err(error),
        }
        let _guard = start_guard(&endpoint, limits.request)?;
        match runtime.block_on(client.request(&request)) {
            Ok(response) => return runtime.block_on(client.wait_result(response, limits.attach)),
            Err(error) if error.code == "host_not_running" => {}
            Err(error) => return Err(error),
        }
        clear_stale(&endpoint)?;
        let mut child = spawn()?;
        let deadline = Instant::now() + limits.attach;
        loop {
            match runtime.block_on(client.request(&request)) {
                Ok(response) => {
                    return runtime.block_on(client.wait_result(
                        response,
                        deadline.saturating_duration_since(Instant::now()),
                    ));
                }
                Err(error) if error.code == "host_not_running" => {}
                Err(error) => return Err(error),
            }
            if let Some(child) = &mut child
                && child
                    .try_wait()
                    .map_err(|_| failure("host_start_failed", "Cannot inspect the new host."))?
                    .is_some()
            {
                return Err(failure(
                    "host_start_failed",
                    "The new menu-bar host exited before startup completed.",
                ));
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    "host_start_timeout",
                    "The new host has not acknowledged startup; no second host was started.",
                ));
            }
            std::thread::sleep(Duration::from_millis(75));
        }
    })();
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
pub fn spawn_detached(executable: &Path, arguments: &[OsString]) -> Result<Child> {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn().map_err(|_| {
        failure(
            "host_start_failed",
            "Could not start the native menu-bar host.",
        )
    })
}
pub fn verify_independent_process() -> Result<()> {
    if unsafe { libc::getsid(0) } != std::process::id() as libc::pid_t {
        return Err(failure(
            "host_not_independent",
            "The background host has no independent process session.",
        ));
    }
    Ok(())
}

async fn run_owned(
    control: OwnedControl,
    host: HostHandle,
    opens: tokio::sync::mpsc::Receiver<u64>,
    driver: Arc<dyn Driver>,
    limits: Limits,
) -> Result<()> {
    let token = CancellationToken::new();
    let listener_host = host.clone();
    let listener_token = token.clone();
    let mut listener =
        tokio::spawn(async move { control.serve(listener_host, limits, listener_token).await });
    let supervisor = lifecycle::supervise(host.clone(), opens, driver, limits);
    tokio::pin!(supervisor);
    let result = tokio::select! {
        result = &mut supervisor => result,
        result = &mut listener => {
            host.stop(); let _ = supervisor.await;
            return match result { Ok(Err(error)) => Err(error), _ => Err(failure("host_control_failed", "The owned control listener stopped unexpectedly.")) };
        }
    };
    token.cancel();
    result.and(listener.await.map_err(|_| {
        failure(
            "host_control_failed",
            "The control listener failed during shutdown.",
        )
    })?)
}
pub async fn run_headless(
    control: OwnedControl,
    specification: StartupSpec,
    driver: Arc<dyn Driver>,
    cancellation: CancellationToken,
    limits: Limits,
) -> Result<()> {
    let (host, opens) = HostHandle::create(specification, cancellation);
    run_owned(control, host, opens, driver, limits).await
}
fn run_native(
    runtime: &tokio::runtime::Runtime,
    options: crate::cli::Options,
    inputs: super::Inputs,
    specification: StartupSpec,
    control: OwnedControl,
    limits: Limits,
) -> Result<i32> {
    let cancellation = CancellationToken::new();
    let mut interrupt;
    let mut terminate;
    {
        let _entered = runtime.enter();
        interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(|_| {
                failure(
                    "signal_error",
                    "Cannot register host interruption handling.",
                )
            })?;
        terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|_| failure("signal_error", "Cannot register host termination handling."))?;
    }
    let signal_cancellation = cancellation.clone();
    let signals = runtime.spawn(async move {
        tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        signal_cancellation.cancel();
    });
    let (host, opens) = HostHandle::create(specification, cancellation.clone());
    let tray = cocoa::Tray::new(host.clone())?;
    let notifier = tray.notifier();
    host.set_wake(notifier.wake());
    let driver = Arc::new(native::NativeDriver {
        options,
        inputs,
        mode: Arc::new(crate::on_demand::Session::default()),
    });
    let task = runtime.spawn(run_owned(control, host, opens, driver, limits));
    let finish = runtime.spawn(async move {
        let result = task
            .await
            .map_err(|_| failure("host_task_failed", "The host task failed."));
        notifier.finish();
        result?
    });
    tray.run();
    cancellation.cancel();
    let result = runtime
        .block_on(finish)
        .map_err(|_| failure("host_task_failed", "Cannot join the host task."))?;
    signals.abort();
    result?;
    Ok(0)
}
pub(super) fn run_desktop(options: crate::cli::Options) -> Result<i32> {
    let runtime = super::runtime()?;
    let result = (|| {
        let inputs = runtime.block_on(native::capture_inputs())?;
        let specification = StartupSpec::new(&options, &inputs)?;
        match claim_desktop(
            &runtime,
            Endpoint::current()?,
            &specification,
            Limits::default(),
        )? {
            DesktopClaim::Reused(response) => Ok(super::response_exit_code(&response)),
            DesktopClaim::Owned(control) => run_native(
                &runtime,
                options,
                inputs,
                specification,
                control,
                Limits::default(),
            ),
        }
    })();
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
pub(super) fn run_entry(entry: Entry) -> Result<i32> {
    let limits = Limits::default();
    match entry {
        Entry::Display(text) => {
            println!("{text}");
            Ok(0)
        }
        Entry::Foreground(arguments) => super::foreground(arguments),
        Entry::Start { arguments, options } => {
            let runtime = super::runtime()?;
            let inputs = runtime.block_on(native::capture_inputs())?;
            runtime.shutdown_timeout(Duration::from_secs(2));
            let specification = StartupSpec::new(&options, &inputs)?;
            let executable = std::env::current_exe().map_err(|_| untrusted())?;
            let mut args = vec![
                OsString::from("--background-host"),
                OsString::from(format!(
                    "--host-specification={}",
                    specification.specification
                )),
            ];
            args.extend(arguments.into_iter().skip(1));
            display_response(&start_with(
                Endpoint::current()?,
                &specification,
                limits,
                || spawn_detached(&executable, &args).map(Some),
            )?)
        }
        Entry::Control(action) => {
            let runtime = super::runtime()?;
            let result = runtime.block_on(async {
                let client = Client::new(Endpoint::current()?, limits);
                let response = match action {
                    TrayAction::Status => client.status().await?,
                    TrayAction::Quit => client.stop_and_wait().await?,
                    TrayAction::Open => {
                        let inputs = native::capture_inputs().await?;
                        let response = client
                            .request(&Request::new(Control::Open {
                                credentials: inputs.credential_stamp(),
                            }))
                            .await?;
                        client
                            .wait_result(response, limits.activation + limits.request)
                            .await?
                    }
                };
                display_response(&response)
            });
            runtime.shutdown_timeout(Duration::from_secs(2));
            result
        }
        Entry::Background {
            options,
            expected_specification,
        } => {
            verify_independent_process()?;
            let runtime = super::runtime()?;
            let result = (|| {
                let inputs = runtime.block_on(native::capture_inputs())?;
                let specification = StartupSpec::new(&options, &inputs)?;
                if specification.specification != expected_specification {
                    return Err(failure(
                        "host_bootstrap_changed",
                        "Startup inputs changed before the child started. No backend was started.",
                    ));
                }
                let control = {
                    let _entered = runtime.enter();
                    OwnedControl::bind(Endpoint::current()?)?
                };
                run_native(&runtime, *options, inputs, specification, control, limits)
            })();
            runtime.shutdown_timeout(Duration::from_secs(2));
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_directory_rejects_links_and_shared_permissions() {
        let root = tempfile::tempdir().unwrap();
        let owned = root.path().join("owned");
        secure_directory(&owned).unwrap();
        std::os::unix::fs::symlink(&owned, root.path().join("link")).unwrap();
        assert!(secure_directory(&root.path().join("link")).is_err());
        std::fs::set_permissions(&owned, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(secure_directory(&owned).is_err());
    }

    #[test]
    fn peer_pair_does_not_authorize_symlinks_or_test_harness_siblings() {
        let root = tempfile::tempdir().unwrap();
        let directory = std::fs::canonicalize(root.path()).unwrap();
        let current = directory.join("github-adapter");
        std::fs::write(&current, b"fixture").unwrap();
        let unrelated = directory.join("unrelated");
        std::fs::write(&unrelated, b"fixture").unwrap();
        let sibling = directory.join("github-adapter-host");
        std::os::unix::fs::symlink(&unrelated, &sibling).unwrap();
        assert_eq!(peer_images(current.clone()).unwrap(), vec![current.clone()]);
        std::fs::remove_file(&sibling).unwrap();
        std::fs::write(&sibling, b"fixture").unwrap();
        assert_eq!(
            peer_images(current.clone()).unwrap(),
            vec![current, sibling]
        );
        assert_eq!(peer_images(unrelated.clone()).unwrap(), vec![unrelated]);
    }

    #[test]
    fn matching_pid_creation_does_not_accept_a_changed_executable_image() {
        let pid = std::process::id();
        let peer = Peer {
            pid,
            created: crate::on_demand::process_created(pid).unwrap().unwrap(),
            image: PathBuf::from("/untrusted-image"),
        };
        assert_eq!(peer.alive().unwrap_err().code, "host_owner_mismatch");
    }
    #[tokio::test]
    async fn control_checks_peer_before_malformed_request_and_keeps_unrelated_paths() {
        let endpoint = Endpoint::isolated(&format!("test-{}", std::process::id())).unwrap();
        let control = OwnedControl::bind(endpoint.clone()).unwrap();
        let client = UnixStream::connect(&endpoint.socket).await.unwrap();
        let (server, _) = control.listener.accept().await.unwrap();
        assert_eq!(
            endpoint.authenticate(&server).unwrap().pid,
            std::process::id()
        );
        assert_eq!(
            endpoint.authenticate(&client).unwrap().pid,
            std::process::id()
        );
        drop(control);
        std::fs::write(&endpoint.socket, b"unrelated").unwrap();
        assert!(clear_stale(&endpoint).is_err());
        assert_eq!(std::fs::read(&endpoint.socket).unwrap(), b"unrelated");
        std::fs::remove_file(&endpoint.socket).unwrap();
    }

    struct FixtureDriver;
    impl Driver for FixtureDriver {
        fn serve(
            &self,
            host: HostHandle,
            cancellation: CancellationToken,
        ) -> futures_util::future::BoxFuture<'_, Result<i32>> {
            Box::pin(async move {
                host.backend_ready("127.0.0.1:5001".parse().unwrap());
                cancellation.cancelled().await;
                Ok(0)
            })
        }
        fn open_codex(&self) -> futures_util::future::BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    #[tokio::test]
    async fn same_user_control_reuses_only_matching_fingerprint_and_drains() {
        let endpoint = Endpoint::isolated(&format!("lifecycle-{}", std::process::id())).unwrap();
        let specification = StartupSpec {
            specification: "a".repeat(64),
            credentials: "b".repeat(64),
            opens_codex: false,
        };
        let control = OwnedControl::bind(endpoint.clone()).unwrap();
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_headless(
            control,
            specification.clone(),
            Arc::new(FixtureDriver),
            cancellation,
            Limits::default(),
        ));
        let client = Client::new(endpoint.clone(), Limits::default());
        let response = client.request(&specification.request()).await.unwrap();
        let ready = client
            .wait_result(response, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(ready.snapshot.state, State::Ready);
        let other = StartupSpec {
            specification: "c".repeat(64),
            ..specification
        };
        let refused = client.request(&other.request()).await.unwrap();
        assert_eq!(refused.error.unwrap().code, "host_configuration_mismatch");
        assert_eq!(client.status().await.unwrap().snapshot.state, State::Ready);
        client.request(&Request::new(Control::Stop)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!endpoint.socket.exists());
    }
}
