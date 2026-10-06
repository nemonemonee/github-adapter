use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adapter_protocol::{AdapterError, Result};
use serde::{Deserialize, Serialize};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, HWND, WAIT_ABANDONED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SYNCHRONIZE};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateMutexW,
    GetCurrentProcess, GetExitCodeProcess, IO_COUNTERS, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, ReleaseMutex, WaitForSingleObject,
};

use super::{
    Activation, Control, Driver, Entry, Fault, HostHandle, Limits, MAX_MESSAGE_BYTES, PeerIdentity,
    PeerPolicy, Request, Response, StartupSpec, State, TrayAction, decode_request, decode_response,
    display_response, failure, lifecycle, native, read_message, tray, write_message,
};
use crate::user_security::{self, Identity, UserSecurity};

// Stable kernel32 APIs not covered by the existing dependency feature gates.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn IsProcessInJob(process: HANDLE, job: HANDLE, result: *mut i32) -> i32;
    fn GetConsoleWindow() -> HWND;
    fn QueryInformationJobObject(
        job: HANDLE,
        class: i32,
        information: *mut std::ffi::c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
    fn CreateJobObjectW(
        attributes: *const windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
        name: *const u16,
    ) -> HANDLE;
    fn SetInformationJobObject(
        job: HANDLE,
        class: i32,
        information: *const std::ffi::c_void,
        length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: HANDLE, process: HANDLE) -> i32;
}

struct Handle(HANDLE);

// Process handles are kernel references, not thread-affine pointers.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn os_failure(code: &str, message: &str) -> AdapterError {
    failure(
        code,
        &format!(
            "{message} (OS code {}).",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ),
    )
}

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

fn process_image(process: HANDLE) -> Result<String> {
    let mut image = vec![0u16; 32768];
    let mut length = image.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &mut length) } == 0
        || length == 0
        || length as usize >= image.len()
    {
        return Err(untrusted());
    }
    let text = String::from_utf16(&image[..length as usize]).map_err(|_| untrusted())?;
    Ok(text)
}

#[derive(Clone)]
pub struct Endpoint {
    pipe: String,
    mutex: String,
    identity: Identity,
    peers: PeerPolicy,
}

impl Endpoint {
    pub fn current() -> Result<Self> {
        Self::isolated("desktop")
    }

    /// Isolation is an in-process harness/library seam, never a public CLI pipe override.
    pub fn isolated(namespace: &str) -> Result<Self> {
        if namespace.is_empty()
            || namespace.len() > 64
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(AdapterError::invalid("Invalid host namespace."));
        }
        let security = UserSecurity::new()?;
        let scope = format!(
            "{}.{}.{}",
            security.identity(),
            security.scope().session,
            namespace
        );
        Ok(Self {
            pipe: format!(r"\\.\pipe\GitHubAdapter.v1.{scope}"),
            mutex: format!(r"Local\GitHubAdapter.Start.v1.{scope}"),
            identity: security.scope().clone(),
            peers: PeerPolicy::new(PeerIdentity {
                image: PathBuf::from(process_image(unsafe { GetCurrentProcess() })?),
                user: security.scope().user.clone(),
                session: security.scope().session,
            })?,
        })
    }

    pub fn pipe_name(&self) -> &str {
        &self.pipe
    }

    fn authenticate(&self, process_id: u32) -> Result<Handle> {
        if process_id == 0 {
            return Err(untrusted());
        }
        let raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                process_id,
            )
        };
        if raw.is_null() {
            return Err(untrusted());
        }
        let process = Handle(raw);
        let identity = user_security::process_identity(process.0).map_err(|_| untrusted())?;
        if identity != self.identity {
            return Err(untrusted());
        }
        if !self.peers.allows(&PeerIdentity {
            image: PathBuf::from(process_image(process.0)?),
            user: identity.user,
            session: identity.session,
        }) {
            return Err(untrusted());
        }
        // Keep this process reference alive through the request, preventing PID-reuse races.
        Ok(process)
    }

    fn pipe(&self, first: bool) -> Result<NamedPipeServer> {
        let mut security = UserSecurity::new()?;
        if security.scope() != &self.identity {
            return Err(untrusted());
        }
        let mut attributes = security.attributes();
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .pipe_mode(PipeMode::Byte)
            .max_instances(32)
            .in_buffer_size(MAX_MESSAGE_BYTES as u32)
            .out_buffer_size(MAX_MESSAGE_BYTES as u32);
        // SECURITY_ATTRIBUTES, descriptor and ACL stay live until CreateNamedPipeW copies them.
        unsafe { options.create_with_security_attributes_raw(&self.pipe, (&mut attributes as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast()) }
            .map_err(|_| os_failure("host_instance_unavailable",
                "Cannot claim the current-user control pipe. An existing or mismatched instance must be resolved explicitly"))
    }
}

fn untrusted() -> AdapterError {
    AdapterError::new(
        409,
        "host_owner_mismatch",
        "The pipe peer's PID, executable image, user, or logon session could not be verified. No control command was sent and no process was stopped.",
    )
}

pub struct OwnedControl {
    endpoint: Endpoint,
    first: NamedPipeServer,
}

impl AsRawHandle for OwnedControl {
    fn as_raw_handle(&self) -> RawHandle {
        self.first.as_raw_handle()
    }
}

impl OwnedControl {
    pub fn bind(endpoint: Endpoint) -> Result<Self> {
        let first = endpoint.pipe(true)?;
        Ok(Self { endpoint, first })
    }

    async fn serve(
        self,
        host: HostHandle,
        limits: Limits,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let endpoint = self.endpoint;
        let mut next = self.first;
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                result = next.connect(), if connections.len() < 24 => {
                    result.map_err(|_| failure("host_control_failed", "The owned control listener failed."))?;
                    // There is always an owned pipe instance, including during a listener handoff.
                    let replacement = endpoint.pipe(false)?;
                    let mut connected = std::mem::replace(&mut next, replacement);
                    let mut process_id = 0;
                    if unsafe { GetNamedPipeClientProcessId(connected.as_raw_handle(), &mut process_id) } == 0 {
                        continue;
                    }
                    let Ok(peer) = endpoint.authenticate(process_id) else { continue; };
                    let host = host.clone();
                    connections.spawn(async move {
                        let _peer = peer;
                        let _ = tokio::time::timeout(limits.request, async {
                            let bytes = read_message(&mut connected).await?;
                            let response = match decode_request(&bytes) {
                                Ok(request) => host.request(&request),
                                Err(error) => host.response(Some(Fault::new(&error.code, &error.message))),
                            };
                            write_message(&mut connected, &response).await
                        }).await;
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        // A stop acknowledgment must be written even if the backend drains immediately.
        let _ = tokio::time::timeout(limits.request, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        drop(next);
        Ok(())
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

    async fn connect(&self) -> Result<(NamedPipeClient, u32, Handle)> {
        loop {
            match ClientOptions::new()
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(&self.endpoint.pipe)
            {
                Ok(pipe) => {
                    let mut process_id = 0;
                    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut process_id) }
                        == 0
                    {
                        return Err(untrusted());
                    }
                    let peer = self.endpoint.authenticate(process_id)?;
                    return Ok((pipe, process_id, peer));
                }
                Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                    return Err(AdapterError::new(
                        404,
                        "host_not_running",
                        "No owned GitHub Adapter host is running in this user session.",
                    ));
                }
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                Err(_) => return Err(untrusted()),
            }
        }
    }

    async fn exchange(&self, request: &Request) -> Result<(Response, Handle)> {
        request.validate()?;
        tokio::time::timeout(self.limits.request, async {
            let (mut pipe, process_id, peer) = self.connect().await?;
            // No request (including stop) is written until the Windows peer is authenticated.
            write_message(&mut pipe, request).await?;
            let response = decode_response(&read_message(&mut pipe).await?)?;
            if response.process_id != process_id { return Err(untrusted()); }
            Ok((response, peer))
        }).await.map_err(|_| AdapterError::new(504, "host_control_timeout",
            "The owned host did not complete the bounded control request. Its state is unknown; no replacement or forced stop was attempted."))?
    }

    pub async fn request(&self, request: &Request) -> Result<Response> {
        self.exchange(request).await.map(|(response, _)| response)
    }

    pub async fn status(&self) -> Result<Response> {
        self.request(&Request::new(Control::Status)).await
    }

    pub async fn wait_result(&self, mut response: Response, budget: Duration) -> Result<Response> {
        let deadline = tokio::time::Instant::now() + budget;
        let process_id = response.process_id;
        loop {
            if response.error.is_some()
                || matches!(response.snapshot.state, State::Failed | State::Stopping)
                || (response.snapshot.state == State::Ready
                    && !matches!(
                        response.snapshot.activation,
                        Activation::Pending | Activation::Opening
                    ))
            {
                return Ok(response);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(response);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let next = match tokio::time::timeout_at(deadline, self.status()).await {
                Ok(next) => next?,
                Err(_) => return Ok(response),
            };
            if next.process_id != process_id {
                return Err(failure(
                    "host_instance_changed",
                    "The owned host changed while waiting. No request was retried on the replacement.",
                ));
            }
            response = next;
        }
    }

    pub async fn stop_and_wait(&self) -> Result<Response> {
        let (mut response, process) = self.exchange(&Request::new(Control::Stop)).await?;
        if response.error.is_some() {
            return Ok(response);
        }
        let budget = self.limits.drain + self.limits.request + Duration::from_secs(3);
        let result = tokio::task::spawn_blocking(move || {
            let wait = unsafe { WaitForSingleObject(process.0, milliseconds(budget)) };
            drop(process);
            wait
        })
        .await
        .map_err(|_| {
            failure(
                "host_stop_unconfirmed",
                "Could not wait for the authenticated host to exit.",
            )
        })?;
        if result != WAIT_OBJECT_0 {
            return Err(AdapterError::new(
                504,
                "host_stop_timeout",
                "Stop was requested, but the authenticated host did not exit before the drain deadline. No process was forcibly terminated.",
            ));
        }
        response.exited = true;
        Ok(response)
    }
}

fn milliseconds(duration: Duration) -> u32 {
    duration.as_millis().min((u32::MAX - 1) as u128) as u32
}

struct StartGuard(Handle);

impl Drop for StartGuard {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.0.0) };
    }
}

fn start_guard(endpoint: &Endpoint, budget: Duration) -> Result<StartGuard> {
    let mut security = UserSecurity::new()?;
    let attributes = security.attributes();
    let raw = unsafe { CreateMutexW(&attributes, 0, wide(&endpoint.mutex).as_ptr()) };
    if raw.is_null() {
        return Err(os_failure(
            "host_start_guard",
            "Cannot acquire the current-user startup guard",
        ));
    }
    let handle = Handle(raw);
    match unsafe { WaitForSingleObject(handle.0, milliseconds(budget)) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(StartGuard(handle)),
        WAIT_TIMEOUT => Err(AdapterError::new(
            409,
            "host_starting",
            "Another launcher still owns the bounded startup transaction. Use status shortly; no additional host was spawned.",
        )),
        _ => Err(os_failure(
            "host_start_guard",
            "Cannot wait for the startup guard",
        )),
    }
}

enum StartGate {
    Existing(Response),
    Held(StartGuard),
}

fn gate_start(
    runtime: &tokio::runtime::Runtime,
    client: &Client,
    endpoint: &Endpoint,
    request: &Request,
    limits: Limits,
) -> Result<StartGate> {
    match runtime.block_on(client.request(request)) {
        Ok(response) => return Ok(StartGate::Existing(response)),
        Err(error) if error.code == "host_not_running" => {}
        Err(error) => return Err(error),
    }
    let guard = start_guard(endpoint, limits.request)?;
    match runtime.block_on(client.request(request)) {
        Ok(response) => Ok(StartGate::Existing(response)),
        Err(error) if error.code == "host_not_running" => Ok(StartGate::Held(guard)),
        Err(error) => Err(error),
    }
}

pub enum DesktopClaim {
    Owned(OwnedControl),
    Reused(Response),
}

/// Same-thread desktop transaction: bind here or reuse. It has no process/job/spawn path.
/// The caller must keep `runtime` alive while running the returned control owner.
pub fn claim_desktop(
    runtime: &tokio::runtime::Runtime,
    endpoint: Endpoint,
    specification: &StartupSpec,
    limits: Limits,
) -> Result<DesktopClaim> {
    let deadline = Instant::now() + limits.attach;
    let client = Client::new(endpoint.clone(), limits);
    match gate_start(
        runtime,
        &client,
        &endpoint,
        &specification.request(),
        limits,
    )? {
        StartGate::Existing(response) => Ok(DesktopClaim::Reused(runtime.block_on(
            client.wait_result(response, deadline.saturating_duration_since(Instant::now())),
        )?)),
        StartGate::Held(guard) => {
            let control = {
                let _entered = runtime.enter();
                OwnedControl::bind(endpoint)?
            };
            drop(guard);
            Ok(DesktopClaim::Owned(control))
        }
    }
}

/// A synchronous transaction keeps the Windows mutex on its acquiring thread.
/// Production passes `spawn_detached`; a fixture may start an in-process owned host and return None.
pub fn start_with(
    endpoint: Endpoint,
    specification: &StartupSpec,
    limits: Limits,
    spawn: impl FnOnce() -> Result<Option<Child>>,
) -> Result<Response> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| failure("runtime_error", "Cannot initialize host control."))?;
    let client = Client::new(endpoint.clone(), limits);
    let request = specification.request();
    let deadline = Instant::now() + limits.attach;
    let response = match gate_start(&runtime, &client, &endpoint, &request, limits)? {
        StartGate::Existing(response) => response,
        StartGate::Held(guard) => {
            let mut child = spawn()?;
            let response = loop {
                match runtime.block_on(client.request(&request)) {
                    Ok(response) => break response,
                    Err(error) if error.code == "host_not_running" => {}
                    Err(error) => {
                        if error.code == "host_control_closed"
                            && let Some(child) = &mut child
                        {
                            // A failed tray may close a pipe already opened by its launcher.
                            for _ in 0..6 {
                                if let Ok(Some(status)) = child.try_wait() {
                                    return Err(bootstrap_failure(status.code()));
                                }
                                std::thread::sleep(Duration::from_millis(25));
                            }
                        }
                        return Err(error);
                    }
                }
                if let Some(child) = &mut child
                    && let Some(status) = child.try_wait().map_err(|_| {
                        failure(
                            "host_start_failed",
                            "Cannot inspect the newly created host.",
                        )
                    })?
                {
                    return Err(bootstrap_failure(status.code()));
                }
                if Instant::now() >= deadline {
                    return Err(AdapterError::new(
                        504,
                        "host_start_timeout",
                        "The new host has not made its control pipe available. No second host or activation was attempted.",
                    ));
                }
                std::thread::sleep(Duration::from_millis(75));
            };
            drop(guard);
            response
        }
    };
    runtime
        .block_on(client.wait_result(response, deadline.saturating_duration_since(Instant::now())))
}

fn bootstrap_failure(status: Option<i32>) -> AdapterError {
    match status {
        Some(70) => failure(
            "tray_unavailable",
            "The native tray/bootstrap failed or exceeded its deadline. The new child exited; no invisible host was retained.",
        ),
        Some(71) => failure(
            "host_bootstrap_changed",
            "Startup inputs changed between launcher and child. No backend, configuration, or activation was started.",
        ),
        Some(72) => failure(
            "host_not_independent",
            "The child could not prove job/console independence and refused to start a backend. Launch the GitHub Adapter desktop shortcut (github-adapter-host.exe) directly; CLI start will not use an attached fallback.",
        ),
        Some(73) => failure(
            "user_security_unavailable",
            "The child could not establish current-user control permissions and exited.",
        ),
        _ => failure(
            "host_start_failed",
            "The newly created host exited before control was available. Detachment, current-user permissions, or tray initialization failed. No existing process was stopped.",
        ),
    }
}

fn in_job(process: HANDLE) -> Result<bool> {
    let mut member = 0;
    if unsafe { IsProcessInJob(process, ptr::null_mut(), &mut member) } == 0 {
        return Err(os_failure(
            "host_detach_unverified",
            "Cannot verify Windows job membership",
        ));
    }
    Ok(member != 0)
}

pub fn verify_independent_process() -> Result<()> {
    if !unsafe { GetConsoleWindow() }.is_null() {
        return Err(failure(
            "host_not_independent",
            "The detached CLI host has an attached console and will not start a backend. Launch the GitHub Adapter desktop shortcut (github-adapter-host.exe) directly.",
        ));
    }
    if in_job(unsafe { GetCurrentProcess() })? {
        return Err(failure(
            "host_not_independent",
            "Windows reports job membership, not its owner or complete ancestry. Detached CLI independence is unproved; no backend was started. Launch the GitHub Adapter desktop shortcut (github-adapter-host.exe) directly.",
        ));
    }
    Ok(())
}

pub fn spawn_detached(executable: &Path, arguments: &[OsString]) -> Result<Child> {
    let mut flags = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    if in_job(unsafe { GetCurrentProcess() })? {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    let mut child = Command::new(executable)
        .args(arguments)
        .creation_flags(flags)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn()
        .map_err(|_| os_failure("host_detach_failed",
            "Windows refused detached CLI host creation. Job breakaway may be prohibited; no attached fallback was started. Launch the GitHub Adapter desktop shortcut (github-adapter-host.exe) directly"))?;
    match in_job(child.as_raw_handle()) {
        Ok(false) => Ok(child),
        result => {
            // This handle is solely the just-created bootstrap child, never a discovered PID.
            let _ = child.kill();
            let _ = child.wait();
            Err(match result {
                Ok(true) => failure(
                    "host_job_unverified",
                    "The child belongs to a Windows job, but this API does not identify its owner or ancestry. Detached CLI independence is unproved; only the new child was stopped. Launch the GitHub Adapter desktop shortcut (github-adapter-host.exe) directly.",
                ),
                Err(error) => error,
                Ok(false) => unreachable!(),
            })
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct JobBasicLimits {
    process_time: i64,
    job_time: i64,
    flags: u32,
    minimum_working_set: usize,
    maximum_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority: u32,
    scheduling: u32,
}

#[repr(C)]
#[derive(Default)]
struct JobExtendedLimits {
    basic: JobBasicLimits,
    io: IO_COUNTERS,
    process_memory: usize,
    job_memory: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobFlags {
    raw: u32,
    kill_on_close: bool,
    breakaway_ok: bool,
    silent_breakaway_ok: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobMembers {
    assigned_count: u32,
    listed_count: u32,
    complete: bool,
    peer_present: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobSnapshot {
    version: u16,
    process_id: u32,
    any_job: Option<bool>,
    membership_error: Option<i32>,
    console_attached: bool,
    immediate_limits: Option<JobFlags>,
    limits_error: Option<i32>,
    immediate_members: Option<JobMembers>,
    members_error: Option<i32>,
}

fn last_code() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

fn job_snapshot(peer: Option<u32>) -> JobSnapshot {
    let mut member = 0;
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), ptr::null_mut(), &mut member) } != 0;
    let membership_error = if ok { None } else { Some(last_code()) };
    let mut snapshot = JobSnapshot {
        version: 1,
        process_id: std::process::id(),
        any_job: ok.then_some(member != 0),
        membership_error,
        console_attached: !unsafe { GetConsoleWindow() }.is_null(),
        immediate_limits: None,
        limits_error: None,
        immediate_members: None,
        members_error: None,
    };
    if snapshot.any_job != Some(true) {
        return snapshot;
    }
    match job_limits(ptr::null_mut()) {
        Ok(limits) => snapshot.immediate_limits = Some(limits),
        Err(error) => snapshot.limits_error = Some(error),
    }
    match job_members(ptr::null_mut(), peer) {
        Ok(members) => snapshot.immediate_members = Some(members),
        Err(error) => snapshot.members_error = Some(error),
    }
    snapshot
}

fn job_limits(job: HANDLE) -> std::result::Result<JobFlags, i32> {
    let mut limits = JobExtendedLimits::default();
    if unsafe {
        QueryInformationJobObject(
            job,
            9,
            (&mut limits as *mut JobExtendedLimits).cast(),
            std::mem::size_of::<JobExtendedLimits>() as u32,
            ptr::null_mut(),
        )
    } == 0
    {
        Err(last_code())
    } else {
        Ok(JobFlags {
            raw: limits.basic.flags,
            kill_on_close: limits.basic.flags & 0x2000 != 0,
            breakaway_ok: limits.basic.flags & 0x800 != 0,
            silent_breakaway_ok: limits.basic.flags & 0x1000 != 0,
        })
    }
}

fn job_members(job: HANDLE, peer: Option<u32>) -> std::result::Result<JobMembers, i32> {
    const MAX_MEMBERS: usize = 1024;
    let bytes = 8 + MAX_MEMBERS * std::mem::size_of::<usize>();
    let mut buffer = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        QueryInformationJobObject(
            job,
            3,
            buffer.as_mut_ptr().cast(),
            bytes as u32,
            ptr::null_mut(),
        )
    } == 0
    {
        Err(last_code())
    } else {
        let counts = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u32>(), 2) };
        let (assigned, listed) = (counts[0], counts[1]);
        if listed as usize <= MAX_MEMBERS {
            let members = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().cast::<u8>().add(8).cast::<usize>(),
                    listed as usize,
                )
            };
            let complete = assigned == listed;
            Ok(JobMembers {
                assigned_count: assigned,
                listed_count: listed,
                complete,
                peer_present: peer.and_then(|pid| {
                    let found = members.contains(&(pid as usize));
                    (found || complete).then_some(found)
                }),
            })
        } else {
            Err(13)
        }
    }
}

fn job_probe(parent: u32, lifetime: bool) -> Result<i32> {
    let _deadline = ProcessWatch::new(Duration::from_secs(if lifetime { 12 } else { 6 }))?;
    let mut gate = [0u8; 1];
    let mut input = std::io::stdin().lock();
    for _ in 0..2 {
        if input.read_exact(&mut gate).is_err() || gate[0] == b'Q' {
            return Ok(0);
        }
        if gate[0] != b'R' {
            return Err(diagnostic_protocol());
        }
        let snapshot = job_snapshot(Some(parent));
        write_job_frame(&snapshot)?;
    }
    Ok(0)
}

fn write_job_frame(value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|_| diagnostic_protocol())?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(diagnostic_protocol());
    }
    let mut output = std::io::stdout().lock();
    output
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|_| output.write_all(&bytes))
        .and_then(|_| output.flush())
        .map_err(|_| diagnostic_protocol())
}

fn diagnostic_protocol() -> AdapterError {
    failure(
        "host_job_diagnostic_protocol",
        "The bounded synthetic job observation could not complete.",
    )
}

struct DiagnosticChild(Child);

impl Drop for DiagnosticChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // Only the child returned by this diagnostic's CreateProcess call is affected.
            let _ = self.0.kill();
            unsafe { WaitForSingleObject(self.0.as_raw_handle(), 1000) };
        }
    }
}

fn read_job_frame<T: serde::de::DeserializeOwned>(output: &mut impl Read) -> Result<T> {
    let mut length = [0; 4];
    output
        .read_exact(&mut length)
        .map_err(|_| diagnostic_protocol())?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        return Err(diagnostic_protocol());
    }
    let mut bytes = vec![0; length];
    output
        .read_exact(&mut bytes)
        .map_err(|_| diagnostic_protocol())?;
    serde_json::from_slice(&bytes).map_err(|_| diagnostic_protocol())
}

fn read_job_snapshot(output: &mut impl Read) -> Result<JobSnapshot> {
    let snapshot: JobSnapshot = read_job_frame(output)?;
    if snapshot.version != 1 || snapshot.process_id == 0 {
        return Err(diagnostic_protocol());
    }
    Ok(snapshot)
}

fn diagnose_jobs(owned_lifetime: bool) -> Result<i32> {
    let _deadline = ProcessWatch::new(Duration::from_secs(if owned_lifetime { 30 } else { 10 }))?;
    let before = job_snapshot(None);
    let mut report = serde_json::json!({
        "version": 1,
        "diagnostic": "windows_job_inheritance",
        "parent_before": before,
        "ancestor_jobs_observed": false,
        "job_handle_owners_observed": false,
        "independence_proved": false,
        "provider_started": false,
        "conclusion": "observation_incomplete",
    });
    if let Some(in_job) = before.any_job {
        let flags = CREATE_NEW_PROCESS_GROUP
            | CREATE_NO_WINDOW
            | if in_job { CREATE_BREAKAWAY_FROM_JOB } else { 0 };
        report["creation_flags"] = flags.into();
        let executable = std::env::current_exe().map_err(|_| diagnostic_protocol())?;
        let spawned = Command::new(executable)
            .args(["--host-job-probe", &std::process::id().to_string()])
            .creation_flags(flags)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Err(error) => {
                report["create_process_error"] = error.raw_os_error().into();
                report["conclusion"] = "create_process_refused".into();
            }
            Ok(child) => {
                let mut child = DiagnosticChild(child);
                let id = child.0.id();
                report["child_process_id"] = id.into();
                let mut output = child.0.stdout.take().ok_or_else(diagnostic_protocol)?;
                let (send, receive) = std::sync::mpsc::sync_channel(1);
                let reader = std::thread::spawn(move || {
                    let _ = send.send(read_job_snapshot(&mut output));
                });
                let gate = child
                    .0
                    .stdin
                    .as_mut()
                    .ok_or_else(diagnostic_protocol)?
                    .write_all(b"R");
                let observed = if gate.is_ok() {
                    receive
                        .recv_timeout(Duration::from_secs(3))
                        .ok()
                        .and_then(std::result::Result::ok)
                        .filter(|snapshot| snapshot.process_id == id)
                } else {
                    None
                };
                let live =
                    unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) } == WAIT_TIMEOUT;
                let after = job_snapshot(Some(id));
                let membership = in_job_of(&child.0);
                let child_membership = membership.as_ref().ok().copied();
                if let Some(snapshot) = &observed {
                    let agrees = snapshot.any_job == child_membership;
                    report["conclusion"] = if !live || !agrees {
                        "observation_incomplete"
                    } else if snapshot.any_job == Some(false) {
                        "jobless_child_observed"
                    } else if snapshot.any_job != Some(true) {
                        "job_query_failed"
                    } else if after.any_job == Some(false) {
                        "child_job_without_launcher_membership"
                    } else if after
                        .immediate_members
                        .as_ref()
                        .is_some_and(|members| members.peer_present == Some(true))
                        || snapshot
                            .immediate_members
                            .as_ref()
                            .is_some_and(|members| members.peer_present == Some(true))
                    {
                        "shared_immediate_job_observed"
                    } else {
                        "unresolved_job_ancestry"
                    }
                    .into();
                }
                report["parent_after"] =
                    serde_json::to_value(after).map_err(|_| diagnostic_protocol())?;
                report["child"] =
                    serde_json::to_value(observed).map_err(|_| diagnostic_protocol())?;
                report["child_any_job_seen_by_parent"] = child_membership.into();
                report["child_membership_query_error"] = membership.err().into();
                report["child_live_during_observation"] = live.into();
                let _ = child.0.stdin.take().map(|mut input| input.write_all(b"Q"));
                let graceful =
                    unsafe { WaitForSingleObject(child.0.as_raw_handle(), 1000) } == WAIT_OBJECT_0;
                if !graceful {
                    let _ = child.0.kill();
                    unsafe { WaitForSingleObject(child.0.as_raw_handle(), 1000) };
                }
                let exit = child.0.try_wait().ok().flatten();
                report["child_exited"] = exit.is_some().into();
                report["child_exit_code"] = exit.and_then(|status| status.code()).into();
                report["forced_fixture_cleanup"] = (!graceful).into();
                let _ = reader.join();
            }
        }
    }
    if owned_lifetime {
        report["owned_lifetime"] = serde_json::json!({
            "scope": "new_fixture_jobs_and_processes_only",
            "ambient_ancestry_proved": false,
            "breakaway_case": owned_job_lifetime(false),
            "reapplied_1800_denied_ancestor_control": owned_job_lifetime(true),
        });
    }
    let output = serde_json::to_string_pretty(&report).map_err(|_| diagnostic_protocol())?;
    if output.len()
        > if owned_lifetime {
            32 * 1024
        } else {
            MAX_MESSAGE_BYTES
        }
    {
        return Err(diagnostic_protocol());
    }
    println!("{output}");
    Ok(0)
}

fn in_job_of(child: &Child) -> std::result::Result<bool, i32> {
    let mut member = 0;
    if unsafe { IsProcessInJob(child.as_raw_handle(), ptr::null_mut(), &mut member) } == 0 {
        Err(last_code())
    } else {
        Ok(member != 0)
    }
}

fn owned_probe_job(flags: u32) -> Result<Handle> {
    let mut security = UserSecurity::new()?;
    let attributes = security.attributes();
    let job = unsafe { CreateJobObjectW(&attributes, ptr::null()) };
    if job.is_null() {
        return Err(os_failure(
            "host_job_probe_create",
            "Cannot create an owned diagnostic job",
        ));
    }
    let job = Handle(job);
    let limits = JobExtendedLimits {
        basic: JobBasicLimits {
            flags,
            ..Default::default()
        },
        ..Default::default()
    };
    if unsafe {
        SetInformationJobObject(
            job.0,
            9,
            (&limits as *const JobExtendedLimits).cast(),
            std::mem::size_of::<JobExtendedLimits>() as u32,
        )
    } == 0
    {
        return Err(os_failure(
            "host_job_probe_limits",
            "Cannot configure an owned diagnostic job",
        ));
    }
    Ok(job)
}

fn member_of(process: HANDLE, job: HANDLE) -> Result<bool> {
    let mut member = 0;
    if unsafe { IsProcessInJob(process, job, &mut member) } == 0 {
        Err(os_failure(
            "host_job_probe_membership",
            "Cannot inspect owned fixture membership",
        ))
    } else {
        Ok(member != 0)
    }
}

fn job_launcher_probe(observer: u32) -> Result<i32> {
    let _deadline = ProcessWatch::new(Duration::from_secs(12))?;
    {
        let mut gate = [0u8; 1];
        if std::io::stdin().lock().read_exact(&mut gate).is_err() || gate[0] != b'R' {
            return Err(diagnostic_protocol());
        }
    }
    let before = job_snapshot(Some(observer));
    let flags = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB;
    let mut report = serde_json::json!({
        "version": 1, "process_id": std::process::id(),
        "launcher_before": before, "creation_flags": flags,
    });
    let child = Command::new(std::env::current_exe().map_err(|_| diagnostic_protocol())?)
        .args([
            "--host-job-probe",
            &std::process::id().to_string(),
            "--lifetime",
        ])
        .creation_flags(flags)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::null())
        .spawn();
    // Only stdio is inherited. The observer's job handles are explicitly non-inheritable.
    match &child {
        Ok(child) => report["child_process_id"] = child.id().into(),
        Err(error) => report["create_process_error"] = error.raw_os_error().into(),
    }
    write_job_frame(&report)?;
    // Keep the creation handle pinned and leave subsequent stdio requests to the leaf.
    std::thread::sleep(Duration::from_secs(10));
    drop(child);
    Ok(0)
}

fn owned_job_lifetime(reapply: bool) -> serde_json::Value {
    match owned_job_lifetime_inner(reapply) {
        Ok(report) => report,
        Err(error) => serde_json::json!({
            "conclusion": "observation_incomplete",
            "error": error.code,
            "detail": error.message,
            "ambient_independence_proved": false,
        }),
    }
}

fn owned_job_lifetime_inner(reapply: bool) -> Result<serde_json::Value> {
    let mut ancestor = if reapply {
        Some(owned_probe_job(0x2000)?)
    } else {
        None
    };
    // The negative control deliberately reapplies an all-breakaway, non-kill job over
    // a retained private kill ancestor. A 0x1800 allowlist must not pass this control.
    let mut job = Some(owned_probe_job(if reapply { 0x1800 } else { 0x2800 })?);
    let mut launcher = DiagnosticChild(
        Command::new(std::env::current_exe().map_err(|_| diagnostic_protocol())?)
            .args(["--host-job-launcher-probe", &std::process::id().to_string()])
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| {
                os_failure(
                    "host_job_probe_spawn",
                    "Cannot create an owned synthetic launcher",
                )
            })?,
    );
    for owned in ancestor.iter().chain(job.iter()) {
        if unsafe { AssignProcessToJobObject(owned.0, launcher.0.as_raw_handle()) } == 0 {
            return Err(os_failure(
                "host_job_probe_assign",
                "Cannot assign the new synthetic launcher to its owned job",
            ));
        }
    }
    let mut output = launcher.0.stdout.take().ok_or_else(diagnostic_protocol)?;
    let mut leaf: Option<Handle> = None;
    std::thread::scope(|scope| {
        let (send, receive) = std::sync::mpsc::sync_channel(3);
        let reader = scope.spawn(move || {
            for _ in 0..3 {
                let value = read_job_frame::<serde_json::Value>(&mut output);
                let failed = value.is_err();
                if send.send(value).is_err() || failed {
                    break;
                }
            }
        });
        let result = (|| -> Result<serde_json::Value> {
            let receive_frame = || {
                receive
                    .recv_timeout(Duration::from_secs(3))
                    .map_err(|_| diagnostic_protocol())?
            };
            let job_handle = job.as_ref().ok_or_else(diagnostic_protocol)?.0;
            launcher
                .0
                .stdin
                .as_mut()
                .ok_or_else(diagnostic_protocol)?
                .write_all(b"R")
                .map_err(|_| diagnostic_protocol())?;
            let launch = receive_frame()?;
            if launch["version"] != 1 || launch["process_id"] != launcher.0.id() {
                return Err(diagnostic_protocol());
            }
            let mut report = serde_json::json!({
                "ambient_independence_proved": false,
                "launcher": launch,
                "launcher_in_explicit_job": member_of(launcher.0.as_raw_handle(), job_handle)?,
                "explicit_launcher_job_limits": job_limits(job_handle).ok(),
                "explicit_launcher_job_members": job_members(job_handle, Some(launcher.0.id())).ok(),
            });
            if launch.get("create_process_error").is_some() {
                report["conclusion"] = "leaf_creation_refused".into();
                return Ok(report);
            }
            let id = launch["child_process_id"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid != 0)
                .ok_or_else(diagnostic_protocol)?;
            // The known launcher retains its child creation handle until this authenticated
            // read-only process reference pins the reported child identity.
            leaf = Some(Endpoint::current()?.authenticate(id)?);
            let process = leaf.as_ref().ok_or_else(diagnostic_protocol)?.0;
            report["leaf_in_launcher_job_before_reapply"] = member_of(process, job_handle)?.into();
            if reapply {
                let assignable =
                    unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, id) };
                if assignable.is_null() {
                    return Err(os_failure(
                        "host_job_probe_assign",
                        "Cannot access the pinned owned leaf for the negative control",
                    ));
                }
                let assignable = Handle(assignable);
                if unsafe { AssignProcessToJobObject(job_handle, assignable.0) } == 0 {
                    return Err(os_failure(
                        "host_job_probe_reapply",
                        "Cannot reapply the owned 0x1800 control job",
                    ));
                }
            }
            launcher
                .0
                .stdin
                .as_mut()
                .ok_or_else(diagnostic_protocol)?
                .write_all(b"R")
                .map_err(|_| diagnostic_protocol())?;
            let before: JobSnapshot =
                serde_json::from_value(receive_frame()?).map_err(|_| diagnostic_protocol())?;
            if before.process_id != id || before.version != 1 {
                return Err(diagnostic_protocol());
            }
            let in_launcher = member_of(process, job_handle)?;
            let in_denied = ancestor
                .as_ref()
                .map(|job| member_of(process, job.0))
                .transpose()?;
            report["leaf_before_close"] =
                serde_json::to_value(before).map_err(|_| diagnostic_protocol())?;
            report["leaf_in_launcher_job_before_close"] = in_launcher.into();
            report["leaf_in_denied_ancestor_before_close"] = in_denied.into();
            report["explicit_launcher_job_members_after_reapply"] =
                serde_json::to_value(job_members(job_handle, Some(id)).ok())
                    .map_err(|_| diagnostic_protocol())?;
            let live = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
            report["leaf_live_before_close"] = live.into();
            if reapply {
                drop(ancestor.take());
            } else {
                drop(job.take());
            }
            let launcher_exited =
                unsafe { WaitForSingleObject(launcher.0.as_raw_handle(), 1000) } == WAIT_OBJECT_0;
            report["launcher_exited_after_owned_job_close"] = launcher_exited.into();
            // Positive evidence requires a NEW reply after the owned launcher has actually exited.
            let sent = launcher
                .0
                .stdin
                .as_mut()
                .ok_or_else(diagnostic_protocol)?
                .write_all(b"R")
                .is_ok();
            let after = if sent {
                receive
                    .recv_timeout(Duration::from_secs(3))
                    .ok()
                    .and_then(std::result::Result::ok)
                    .and_then(|value| serde_json::from_value::<JobSnapshot>(value).ok())
                    .filter(|snapshot| snapshot.version == 1 && snapshot.process_id == id)
            } else {
                None
            };
            let fresh_reply = after.is_some();
            report["fresh_leaf_reply_after_close"] = fresh_reply.into();
            report["leaf_after_close"] =
                serde_json::to_value(after).map_err(|_| diagnostic_protocol())?;
            let leaf_exited = unsafe { WaitForSingleObject(process, 1000) } == WAIT_OBJECT_0;
            let mut code = 0;
            report["leaf_exited"] = leaf_exited.into();
            let exit_queried =
                leaf_exited && unsafe { GetExitCodeProcess(process, &mut code) } != 0;
            report["leaf_exit_code"] = if exit_queried {
                serde_json::json!(code)
            } else {
                serde_json::Value::Null
            };
            report["conclusion"] = if reapply
                && live
                && launcher_exited
                && in_denied == Some(true)
                && leaf_exited
                && !fresh_reply
            {
                "reapplied_1800_does_not_exclude_kill_ancestor"
            } else if !reapply
                && live
                && launcher_exited
                && !in_launcher
                && fresh_reply
                && exit_queried
                && code == 0
            {
                "owned_launcher_job_independence_observed"
            } else {
                "observation_incomplete"
            }
            .into();
            Ok(report)
        })();
        let _ = launcher
            .0
            .stdin
            .take()
            .map(|mut input| input.write_all(b"Q"));
        drop(ancestor.take());
        drop(job.take());
        if launcher.0.try_wait().ok().flatten().is_none() {
            let _ = launcher.0.kill();
            unsafe { WaitForSingleObject(launcher.0.as_raw_handle(), 1000) };
        }
        // EOF/quit and the leaf's own deadline cover cleanup even after its launcher is gone.
        let _ = reader.join();
        result
    })
}

async fn run_owned(
    control: OwnedControl,
    specification: StartupSpec,
    driver: Arc<dyn Driver>,
    cancellation: CancellationToken,
    limits: Limits,
    host_override: Option<(HostHandle, tokio::sync::mpsc::Receiver<u64>)>,
) -> Result<()> {
    let (host, opens) =
        host_override.unwrap_or_else(|| HostHandle::create(specification, cancellation.clone()));
    let control_cancel = CancellationToken::new();
    let _control_guard = control_cancel.clone().drop_guard();
    let control_host = host.clone();
    let control_token = control_cancel.clone();
    let mut listener =
        tokio::spawn(async move { control.serve(control_host, limits, control_token).await });
    let supervisor = lifecycle::supervise(host.clone(), opens, driver, limits);
    tokio::pin!(supervisor);
    let result = tokio::select! {
        result = &mut supervisor => result,
        result = &mut listener => {
            host.fail(Fault::new("host_control_failed", "The authenticated control listener failed. The owned backend is shutting down."));
            host.stop();
            let _ = (&mut supervisor).await;
            return match result {
                Ok(Err(error)) => Err(error),
                _ => Err(failure("host_control_failed", "The owned control task ended unexpectedly.")),
            };
        }
    };
    control_cancel.cancel();
    let listener_result = listener.await.map_err(|_| {
        failure(
            "host_control_failed",
            "The owned control task failed during shutdown.",
        )
    })?;
    result.and(listener_result)
}

/// Headless composition for synthetic drivers. Production background entry always requires a tray.
pub async fn run_headless(
    control: OwnedControl,
    specification: StartupSpec,
    driver: Arc<dyn Driver>,
    cancellation: CancellationToken,
    limits: Limits,
) -> Result<()> {
    run_owned(control, specification, driver, cancellation, limits, None).await
}

pub(super) fn run_entry(entry: Entry) -> Result<i32> {
    let limits = Limits::default();
    match entry {
        Entry::Display(text) => {
            println!("{text}");
            Ok(0)
        }
        Entry::Foreground(arguments) => {
            match arguments.get(1).and_then(|argument| argument.to_str()) {
                Some("--diagnose-host-jobs") if arguments.len() == 2 => diagnose_jobs(false),
                Some("--diagnose-host-jobs")
                    if arguments.len() == 3 && arguments[2] == "--owned-lifetime" =>
                {
                    diagnose_jobs(true)
                }
                Some("--host-job-probe")
                    if arguments.len() == 3
                        || (arguments.len() == 4 && arguments[3] == "--lifetime") =>
                {
                    let parent = arguments[2]
                        .to_str()
                        .and_then(|text| text.parse::<u32>().ok())
                        .filter(|pid| *pid != 0)
                        .ok_or_else(|| AdapterError::invalid("Invalid synthetic job probe."))?;
                    job_probe(parent, arguments.len() == 4)
                }
                Some("--host-job-launcher-probe") if arguments.len() == 3 => {
                    let observer = arguments[2]
                        .to_str()
                        .and_then(|text| text.parse::<u32>().ok())
                        .filter(|pid| *pid != 0)
                        .ok_or_else(|| AdapterError::invalid("Invalid owned launcher probe."))?;
                    job_launcher_probe(observer)
                }
                _ => super::foreground(arguments),
            }
        }
        Entry::Start { arguments, options } => {
            let runtime = super::runtime()?;
            let inputs = runtime.block_on(native::capture_inputs());
            runtime.shutdown_timeout(Duration::from_secs(2));
            let specification = StartupSpec::new(&options, &inputs?)?;
            let executable = std::env::current_exe().map_err(|_| {
                failure(
                    "host_identity_unavailable",
                    "Cannot locate the installed binary.",
                )
            })?;
            let mut child_arguments = vec![
                OsString::from("--background-host"),
                OsString::from(format!(
                    "--host-specification={}",
                    specification.specification
                )),
            ];
            child_arguments.extend(arguments.into_iter().skip(1));
            let response = start_with(Endpoint::current()?, &specification, limits, || {
                spawn_detached(&executable, &child_arguments).map(Some)
            })?;
            display_response(&response)
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
        } => background(*options, &expected_specification, limits),
    }
}

fn background(
    options: crate::cli::Options,
    expected_specification: &str,
    limits: Limits,
) -> Result<i32> {
    verify_independent_process()?;
    let runtime = super::runtime()?;
    let result = background_in(&runtime, options, expected_specification, limits);
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

fn background_in(
    runtime: &tokio::runtime::Runtime,
    options: crate::cli::Options,
    expected_specification: &str,
    limits: Limits,
) -> Result<i32> {
    let watchdog = ProcessWatch::new(limits.attach)?;
    let startup = NativeStartup::capture(runtime, options)?;
    if startup.specification.specification != expected_specification {
        return Err(failure(
            "host_bootstrap_changed",
            "Startup inputs changed between the launcher and child. No backend, configuration, or activation was started. Run start again explicitly.",
        ));
    }
    let endpoint = Endpoint::current()?;
    let control = {
        let _entered = runtime.enter();
        OwnedControl::bind(endpoint)?
    };
    run_native_host(runtime, startup, control, limits, watchdog)
}

pub(super) fn run_desktop(options: crate::cli::Options) -> Result<i32> {
    let limits = Limits::default();
    let runtime = super::runtime()?;
    let result = (|| {
        let coordination =
            ProcessWatch::new(limits.attach + limits.request + Duration::from_secs(5))?;
        let startup = NativeStartup::capture(&runtime, options)?;
        let claim = claim_desktop(
            &runtime,
            Endpoint::current()?,
            &startup.specification,
            limits,
        )?;
        drop(coordination);
        match claim {
            DesktopClaim::Reused(response) => Ok(super::response_exit_code(&response)),
            DesktopClaim::Owned(control) => run_native_host(
                &runtime,
                startup,
                control,
                limits,
                ProcessWatch::new(limits.attach)?,
            ),
        }
    })();
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

struct NativeStartup {
    options: crate::cli::Options,
    inputs: super::Inputs,
    specification: StartupSpec,
}

impl NativeStartup {
    fn capture(runtime: &tokio::runtime::Runtime, options: crate::cli::Options) -> Result<Self> {
        let inputs = runtime.block_on(native::capture_inputs())?;
        let specification = StartupSpec::new(&options, &inputs)?;
        Ok(Self {
            options,
            inputs,
            specification,
        })
    }
}

fn run_native_host(
    runtime: &tokio::runtime::Runtime,
    startup: NativeStartup,
    control: OwnedControl,
    limits: Limits,
    watchdog: ProcessWatch,
) -> Result<i32> {
    let NativeStartup {
        options,
        inputs,
        specification,
    } = startup;
    let cancellation = CancellationToken::new();
    let (host, opens) = HostHandle::create(specification.clone(), cancellation.clone());
    let tray = tray::Tray::new(host.clone())?;
    let notifier = tray.notifier();
    host.set_wake(notifier.wake());
    let driver = Arc::new(native::NativeDriver {
        options,
        inputs,
        mode: Arc::new(crate::on_demand::Session::default()),
    });
    watchdog.running();
    let task = runtime.spawn(run_owned(
        control,
        specification,
        driver,
        cancellation.clone(),
        limits,
        Some((host.clone(), opens)),
    ));
    // A separate join observer also wakes the UI if the owned runtime task panics.
    let exit_deadline = watchdog.sender();
    let finish = runtime.spawn(async move {
        let result = match task.await {
            Ok(result) => result,
            Err(_) => Err(failure("host_task_failed", "The owned host task failed.")),
        };
        let _ = exit_deadline.send(WatchEvent::Exiting);
        notifier.finish();
        result
    });
    let ui_result = tray.run();
    if ui_result.is_err() {
        host.ui_failure();
    }
    cancellation.cancel();
    let result = runtime
        .block_on(finish)
        .map_err(|_| failure("host_task_failed", "Could not join the owned host task."));
    ui_result?;
    result??;
    Ok(0)
}

enum WatchEvent {
    Running,
    Exiting,
    Finished,
}

struct ProcessWatch {
    done: std::sync::mpsc::Sender<WatchEvent>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl ProcessWatch {
    fn new(budget: Duration) -> Result<Self> {
        let (done, receiver) = std::sync::mpsc::channel();
        let task = std::thread::Builder::new()
            .name("adapter-process-deadline".into())
            .spawn(move || {
                let mut deadline = Some(budget);
                loop {
                    let event = match deadline {
                        Some(budget) => receiver.recv_timeout(budget),
                        None => receiver
                            .recv()
                            .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
                    };
                    match event {
                        Ok(WatchEvent::Running) => deadline = None,
                        Ok(WatchEvent::Exiting) => deadline = Some(Duration::from_secs(2)),
                        Ok(WatchEvent::Finished)
                        | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // Exit only this process, either before backend startup or after its
                            // owned work drained. A hung shell API cannot retain an invisible host.
                            std::process::exit(70);
                        }
                    }
                }
            })
            .map_err(|_| {
                failure(
                    "host_bootstrap_unavailable",
                    "Cannot establish the bounded host bootstrap.",
                )
            })?;
        Ok(Self {
            done,
            task: Some(task),
        })
    }

    fn running(&self) {
        let _ = self.done.send(WatchEvent::Running);
    }

    fn sender(&self) -> std::sync::mpsc::Sender<WatchEvent> {
        self.done.clone()
    }
}

impl Drop for ProcessWatch {
    fn drop(&mut self) {
        let _ = self.done.send(WatchEvent::Finished);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
