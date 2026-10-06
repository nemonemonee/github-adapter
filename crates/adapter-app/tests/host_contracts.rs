#![cfg(windows)]

use std::ffi::{OsString, c_void};
use std::net::SocketAddr;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use adapter_app::cli::{self, Options, Parsed};
use adapter_app::host::{
    self, Activation, Client, Control, DesktopClaim, Driver, Endpoint, Entry, HostHandle, Inputs,
    Limits, MAX_MESSAGE_BYTES, OwnedControl, PeerIdentity, PeerPolicy, Request, StartupSpec, State,
    TrayAction,
};
use adapter_app::listener;
use adapter_protocol::{AdapterError, Result};
use futures_util::future::BoxFuture;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    EqualSid, GetAce, GetAclInformation, GetKernelObjectSecurity, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetTokenInformation, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, GetCurrentProcess,
    IO_COUNTERS, OpenProcessToken, WaitForSingleObject,
};

static NEXT: AtomicUsize = AtomicUsize::new(1);
const SECRET: &str = "synthetic-host-token-not-a-real-account";

#[test]
fn bootstrap_exit_codes_preserve_foreground_argument_failure_codes() {
    for (code, expected) in [
        ("tray_unavailable", 70),
        ("host_bootstrap_changed", 71),
        ("host_not_independent", 72),
        ("user_security_unavailable", 73),
    ] {
        assert_eq!(
            host::error_exit_code(&AdapterError::new(500, code, "")),
            expected
        );
    }
    assert_eq!(host::error_exit_code(&AdapterError::invalid("invalid")), 2);
}

fn namespace() -> String {
    format!(
        "contract-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn parsed(args: &[&str]) -> Options {
    match cli::parse_from(std::iter::once("adapter").chain(args.iter().copied())).unwrap() {
        Parsed::Run(options) => *options,
        _ => panic!("expected options"),
    }
}

fn options(open: bool) -> Options {
    if open {
        parsed(&["auto", "--port", "0"])
    } else {
        parsed(&["serve", "--provider", "mai", "--port", "0"])
    }
}

fn spec(open: bool) -> StartupSpec {
    StartupSpec::new(&options(open), &Inputs::default()).unwrap()
}

#[test]
fn desktop_api_parses_without_foreground_side_effects_and_preserves_defaults() {
    let parse = |args: &[&str]| host::parse_desktop_from(args.iter().map(OsString::from));
    for args in [
        vec!["github-adapter-host"],
        vec!["github-adapter-host", "start"],
        vec!["github-adapter-host", "auto"],
    ] {
        let Parsed::Run(options) = parse(&args).unwrap() else {
            panic!("expected desktop options")
        };
        let default = parsed(&[]);
        assert_eq!(
            StartupSpec::new(&options, &Inputs::default()).unwrap(),
            StartupSpec::new(&default, &Inputs::default()).unwrap()
        );
    }
    let Parsed::Run(options) =
        parse(&["gui", "serve", "--provider", "mai", "--port", "0"]).unwrap()
    else {
        panic!("expected manual serving options")
    };
    assert!(!options.setup);
    assert!(options.launch.is_none());
    for command in [
        "login",
        "logout",
        "account",
        "setup",
        "restore",
        "doctor",
        "apps",
        "recover",
        "models",
        "status",
        "open",
        "stop",
        "--background-host",
        "--diagnose-host-jobs",
    ] {
        assert!(
            host::run_desktop_from(["gui", command].into_iter().map(OsString::from)).is_err(),
            "{command} must not execute from the GUI API"
        );
    }
    assert!(parse(&["gui", "serve", "--choose-provider"]).is_err());
    assert!(parse(&["gui", "serve", "--launch", "chatgpt"]).is_err());
    for flag in ["--help", "--version"] {
        assert_eq!(
            host::run_desktop_from(["gui", flag].into_iter().map(OsString::from)).unwrap(),
            0
        );
    }
}

#[test]
fn direct_desktop_entry_has_no_detach_or_job_membership_path() {
    let source = include_str!("..\\src\\host\\platform.rs");
    let direct = source
        .split("pub(super) fn run_desktop(")
        .nth(1)
        .unwrap()
        .split("struct NativeStartup")
        .next()
        .unwrap();
    let claim = source
        .split("pub fn claim_desktop(")
        .nth(1)
        .unwrap()
        .split("pub fn start_with(")
        .next()
        .unwrap();
    let composition = source
        .split("fn run_native_host(")
        .nth(1)
        .unwrap()
        .split("enum WatchEvent")
        .next()
        .unwrap();
    for body in [direct, claim, composition] {
        for forbidden in [
            "spawn_detached(",
            "verify_independent_process(",
            "in_job(",
            "IsProcessInJob(",
            "CREATE_BREAKAWAY_FROM_JOB",
            "Command::new(",
        ] {
            assert!(
                !body.contains(forbidden),
                "direct path includes {forbidden}"
            );
        }
    }
    assert!(direct.contains("claim_desktop("));
    assert!(direct.contains("run_native_host("));
}

fn peer(image: &std::path::Path, user: &str, session: u32) -> PeerIdentity {
    PeerIdentity {
        image: image.to_path_buf(),
        user: user.into(),
        session,
    }
}

#[test]
fn paired_release_paths_accept_both_roles_but_reject_other_paths_and_scopes() {
    let root = tempfile::Builder::new()
        .prefix("host-peer-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let cli = root.path().join("github-adapter.exe");
    let gui = root.path().join("github-adapter-host.exe");
    std::fs::write(&cli, b"synthetic-cli").unwrap();
    std::fs::write(&gui, b"synthetic-gui").unwrap();
    for current in [&cli, &gui] {
        let policy = PeerPolicy::new(peer(current, "synthetic-user-a", 7)).unwrap();
        for accepted in [&cli, &gui] {
            assert!(policy.allows(&peer(accepted, "synthetic-user-a", 7)));
            assert!(!policy.allows(&peer(accepted, "synthetic-user-b", 7)));
            assert!(!policy.allows(&peer(accepted, "synthetic-user-a", 8)));
        }
        for name in [
            "github-adapter-host.exe.bak",
            "github-adapter-host-evil.exe",
            "github-adapter-host2.exe",
            "arbitrary.exe",
        ] {
            let wrong = root.path().join(name);
            std::fs::write(&wrong, b"not-a-role").unwrap();
            assert!(!policy.allows(&peer(&wrong, "synthetic-user-a", 7)));
        }
        let other = root.path().join("other-bin");
        std::fs::create_dir_all(&other).unwrap();
        let wrong = other.join("github-adapter-host.exe");
        if !wrong.exists() {
            std::fs::hard_link(&gui, &wrong).unwrap();
        }
        assert!(!policy.allows(&peer(&wrong, "synthetic-user-a", 7)));
        let prefix = root
            .path()
            .join("github-adapter-host.exe")
            .join("child.exe");
        assert!(!policy.allows(&peer(&prefix, "synthetic-user-a", 7)));
    }
    let fixture = root.path().join("host-contract-fixture.exe");
    std::fs::write(&fixture, b"fixture").unwrap();
    let policy = PeerPolicy::new(peer(&fixture, "synthetic-user-a", 7)).unwrap();
    assert!(policy.allows(&peer(&fixture, "synthetic-user-a", 7)));
    assert!(!policy.allows(&peer(&cli, "synthetic-user-a", 7)));
    assert!(!policy.allows(&peer(&gui, "synthetic-user-a", 7)));
}

fn limits() -> Limits {
    Limits {
        request: Duration::from_secs(1),
        attach: Duration::from_secs(4),
        startup: Duration::from_secs(3),
        activation: Duration::from_secs(1),
        drain: Duration::from_secs(2),
    }
}

#[derive(Default)]
struct Fake {
    open_initially: bool,
    startup_delay: Duration,
    open_delay: Duration,
    drain_delay: Duration,
    failed_backend: bool,
    failed_open: bool,
    pending_start: bool,
    pending_open: bool,
    starts: AtomicUsize,
    opens: AtomicUsize,
    drains: AtomicUsize,
    address: Mutex<Option<SocketAddr>>,
    host: Mutex<Option<HostHandle>>,
}

impl Driver for Fake {
    fn serve(
        &self,
        host: HostHandle,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<i32>> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            *self.host.lock().unwrap() = Some(host.clone());
            let listener = listener::bind_loopback("127.0.0.1:0".parse().unwrap())?;
            let address = listener.local_addr().unwrap();
            *self.address.lock().unwrap() = Some(address);
            let listener = TcpListener::from_std(listener).unwrap();
            if self.pending_start {
                cancellation.cancelled().await;
                self.drains.fetch_add(1, Ordering::SeqCst);
                return Ok(0);
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(0),
                _ = tokio::time::sleep(self.startup_delay) => {}
            }
            if self.failed_backend {
                return Err(AdapterError::new(
                    502,
                    SECRET,
                    format!("backend leaked {SECRET}\nsettings"),
                ));
            }
            host.backend_ready(address);
            if self.open_initially {
                host.initial_open()?;
            }
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        let _ = tokio::time::timeout(Duration::from_millis(250),
                            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nsynthetic")).await;
                    }
                }
            }
            tokio::time::sleep(self.drain_delay).await;
            self.drains.fetch_add(1, Ordering::SeqCst);
            drop(listener);
            Ok(0)
        })
    }

    fn open_codex(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if self.pending_open {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(self.open_delay).await;
            if self.failed_open {
                return Err(AdapterError::new(502, SECRET, SECRET));
            }
            Ok(())
        })
    }
}

struct Fixture {
    endpoint: Endpoint,
    client: Client,
    specification: StartupSpec,
    driver: Arc<Fake>,
    cancellation: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
}

impl Fixture {
    fn new(driver: Fake, limits: Limits) -> Self {
        let specification = spec(driver.open_initially);
        let endpoint = Endpoint::isolated(&namespace()).unwrap();
        let control = OwnedControl::bind(endpoint.clone()).unwrap();
        let cancellation = CancellationToken::new();
        let driver = Arc::new(driver);
        let task = tokio::spawn(host::run_headless(
            control,
            specification.clone(),
            driver.clone(),
            cancellation.clone(),
            limits,
        ));
        Self {
            client: Client::new(endpoint.clone(), limits),
            endpoint,
            specification,
            driver,
            cancellation,
            task: Some(task),
        }
    }

    async fn ready(&self) -> host::Response {
        let response = self.client.status().await.unwrap();
        self.client
            .wait_result(response, Duration::from_secs(2))
            .await
            .unwrap()
    }

    async fn finish(mut self) {
        self.cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(4), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let error = self.client.status().await.unwrap_err();
            if error.code == "host_not_running" {
                break;
            }
            // Windows may finish cancelling an overlapped pipe handle on the next I/O turn.
            assert_eq!(error.code, "host_control_closed");
            assert!(
                Instant::now() < deadline,
                "owned control pipe did not disappear"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[test]
fn entry_routes_only_requested_background_commands() {
    let entry = |args: &[&str]| host::entry_from(args.iter().map(OsString::from));
    for args in [&["adapter"][..], &["adapter", "start"][..]] {
        match entry(args).unwrap() {
            Entry::Start { options, .. } => {
                assert!(options.setup);
                assert_eq!(options.launch, Some(cli::Launch::Codex));
            }
            _ => panic!("default must start the host"),
        }
    }
    assert!(matches!(
        entry(&["adapter", "serve"]).unwrap(),
        Entry::Foreground(_)
    ));
    assert!(matches!(
        entry(&["adapter", "auto"]).unwrap(),
        Entry::Foreground(_)
    ));
    assert!(matches!(
        entry(&["adapter", "login"]).unwrap(),
        Entry::Foreground(_)
    ));
    assert!(matches!(
        entry(&["adapter", "status"]).unwrap(),
        Entry::Control(TrayAction::Status)
    ));
    assert!(matches!(
        entry(&["adapter", "open"]).unwrap(),
        Entry::Control(TrayAction::Open)
    ));
    assert!(matches!(
        entry(&["adapter", "stop"]).unwrap(),
        Entry::Control(TrayAction::Quit)
    ));
    let fingerprint = format!("--host-specification={}", "a".repeat(64));
    match entry(&[
        "adapter",
        "--background-host",
        &fingerprint,
        "serve",
        "--provider",
        "mai",
        "--port",
        "0",
    ])
    .unwrap()
    {
        Entry::Background { options, .. } => assert_eq!(options.address.port(), 0),
        _ => panic!("internal marker was not removed"),
    }
    for args in [
        vec!["adapter", "start", "login"],
        vec!["adapter", "start", "setup"],
        vec!["adapter", "start", "serve", "--choose-provider"],
        vec!["adapter", "start", "serve", "--launch", "chatgpt"],
        vec!["adapter", "--background-host", "account"],
        vec!["adapter", "serve", "--background-host"],
        vec!["adapter", "stop", "--port", "1234"],
    ] {
        assert!(entry(&args).is_err(), "{args:?}");
    }
    for args in [
        &["adapter", "--help"][..],
        &["adapter", "start", "--help"],
        &["adapter", "serve", "--help"],
    ] {
        let Entry::Display(text) = entry(args).unwrap() else {
            panic!("expected help");
        };
        assert!(text.contains("github-adapter-host.exe"));
        assert!(text.contains("CLI start reuses a compatible host"));
        assert!(!text.contains("not implemented"));
    }
}

#[test]
fn configuration_and_credential_fingerprints_are_normalized_and_secret_free() {
    let inputs = Inputs::new(
        [(cli::TOKEN_ENV.into(), SECRET.into())],
        Some(format!(
            r#"{{"version":1,"login":"synthetic","token":"{SECRET}"}}"#
        )),
    );
    let mut options = options(false);
    options.github_account = Some("SYNTHETIC".into());
    let first = StartupSpec::new(&options, &inputs).unwrap();
    options.github_account = Some("synthetic".into());
    options.upstream = "http://127.0.0.1:5000/".into();
    assert_eq!(first, StartupSpec::new(&options, &inputs).unwrap());
    let encoded = String::from_utf8(host::encode_message(&first.request()).unwrap()).unwrap();
    assert!(!encoded.contains(SECRET));
    assert!(!encoded.contains("synthetic"));
    options.address.set_port(23457);
    assert_ne!(
        first.specification,
        StartupSpec::new(&options, &inputs).unwrap().specification
    );
    let changed = Inputs::new(
        [(cli::TOKEN_ENV.into(), "different-fixture-token".into())],
        None,
    );
    assert_ne!(first.credentials, changed.credential_stamp());
    let saved_changed = Inputs::new(
        [(cli::TOKEN_ENV.into(), SECRET.into())],
        Some("different".into()),
    );
    assert_ne!(first.credentials, saved_changed.credential_stamp());
    let root = tempfile::Builder::new()
        .prefix("host-path-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    options.models_cache = Some(root.path().join("models.json"));
    let first = StartupSpec::new(&options, &Inputs::default()).unwrap();
    options.models_cache = Some(root.path().join(".").join("models.json"));
    assert_eq!(
        first,
        StartupSpec::new(&options, &Inputs::default()).unwrap()
    );
}

#[test]
fn protocol_rejects_unknown_versions_fields_actions_and_sizes() {
    let request = spec(false).request();
    assert_eq!(
        request,
        host::decode_request(&host::encode_message(&request).unwrap()).unwrap()
    );
    for value in [
        json!({"version":2,"command":{"action":"status"}}),
        json!({"version":1,"command":{"action":"status","execute":"anything"}}),
        json!({"version":1,"command":{"action":"execute","path":"not-allowed"}}),
        json!({"version":1,"command":{"action":"open","credentials":"short"}}),
        json!({"version":1,"command":{"action":"status"},"extra":true}),
        json!({"version":1,"command":{"action":"start","specification":"a".repeat(64),"credentials":"z".repeat(64)}}),
    ] {
        assert!(host::decode_request(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    assert!(
        host::decode_request(br#"{"version":1,"version":1,"command":{"action":"status"}}"#)
            .is_err()
    );
    assert!(host::decode_request(&vec![b' '; MAX_MESSAGE_BYTES + 1]).is_err());
    assert!(host::encode_message(&"x".repeat(MAX_MESSAGE_BYTES)).is_err());
    assert!(host::decode_request(b"").is_err());
}

#[tokio::test]
async fn frame_size_is_rejected_before_body_allocation_or_wait() {
    let (mut write, mut read) = tokio::io::duplex(16);
    write
        .write_u32_le((MAX_MESSAGE_BYTES + 1) as u32)
        .await
        .unwrap();
    let result =
        tokio::time::timeout(Duration::from_millis(100), host::read_message(&mut read)).await;
    assert_eq!(result.unwrap().unwrap_err().code, "host_protocol_invalid");
}

#[tokio::test]
async fn first_instance_has_a_protected_current_user_only_dacl() {
    let endpoint = Endpoint::isolated(&namespace()).unwrap();
    let first = OwnedControl::bind(endpoint.clone()).unwrap();
    assert!(OwnedControl::bind(endpoint).is_err());
    let mut needed = 0;
    unsafe {
        GetKernelObjectSecurity(
            first.as_raw_handle(),
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    assert!(needed > 0 && needed < 65536);
    let mut descriptor = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetKernelObjectSecurity(
                first.as_raw_handle(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        },
        0
    );
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl: *mut ACL = ptr::null_mut();
    assert_ne!(
        unsafe {
            GetSecurityDescriptorDacl(
                descriptor.as_mut_ptr().cast(),
                &mut present,
                &mut acl,
                &mut defaulted,
            )
        },
        0
    );
    assert_ne!(present, 0);
    assert_eq!(defaulted, 0);
    assert!(!acl.is_null());
    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe {
            GetSecurityDescriptorControl(
                descriptor.as_mut_ptr().cast(),
                &mut control,
                &mut revision,
            )
        },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);
    let mut info = ACL_SIZE_INFORMATION::default();
    assert_ne!(
        unsafe {
            GetAclInformation(
                acl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        },
        0
    );
    assert_eq!(info.AceCount, 1);
    let mut ace = ptr::null_mut();
    assert_ne!(unsafe { GetAce(acl, 0, &mut ace) }, 0);
    let ace = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
    assert_eq!(ace.Header.AceType, 0);
    assert!(matches!(ace.Mask, 0x1000_0000 | 0x001f_01ff));
    let mut token = ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let token = KernelHandle(token);
    let mut length = 0;
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length) };
    let mut user = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                user.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        },
        0
    );
    let sid = unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    assert_ne!(
        unsafe { EqualSid(ptr::addr_of!(ace.SidStart).cast_mut().cast(), sid) },
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_concurrent_starts_share_one_host_and_one_initial_activation() {
    const CALLERS: usize = 100;
    let budgets = Limits::default();
    let endpoint = Endpoint::isolated(&namespace()).unwrap();
    let specification = spec(true);
    let driver = Arc::new(Fake {
        open_initially: true,
        startup_delay: Duration::from_millis(100),
        open_delay: Duration::from_millis(200),
        ..Default::default()
    });
    let spawned = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(CALLERS));
    let cancellation = CancellationToken::new();
    let workers = Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Handle::current();
    let mut callers = Vec::new();
    for _ in 0..CALLERS {
        let (endpoint, specification, driver, spawned, barrier, cancellation, workers, runtime) = (
            endpoint.clone(),
            specification.clone(),
            driver.clone(),
            spawned.clone(),
            barrier.clone(),
            cancellation.clone(),
            workers.clone(),
            runtime.clone(),
        );
        callers.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            host::start_with(endpoint.clone(), &specification.clone(), budgets, || {
                spawned.fetch_add(1, Ordering::SeqCst);
                let control = {
                    let _entered = runtime.enter();
                    OwnedControl::bind(endpoint)?
                };
                workers
                    .lock()
                    .unwrap()
                    .push(runtime.spawn(host::run_headless(
                        control,
                        specification,
                        driver,
                        cancellation,
                        budgets,
                    )));
                Ok(None)
            })
        }));
    }
    let mut responses = Vec::new();
    for caller in callers {
        responses.push(caller.await.unwrap().unwrap());
    }
    cancellation.cancel();
    let tasks = std::mem::take(&mut *workers.lock().unwrap());
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(responses.len(), CALLERS);
    assert_eq!(spawned.load(Ordering::SeqCst), 1);
    assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
    assert_eq!(driver.opens.load(Ordering::SeqCst), 1);
    for response in &responses {
        assert_eq!(response.snapshot.state, State::Ready);
        assert_eq!(response.snapshot.activation, Activation::Acknowledged);
        assert!(response.error.is_none(), "{response:?}");
        assert_eq!(response.snapshot.address, responses[0].snapshot.address);
    }
}

#[tokio::test]
async fn ready_does_not_claim_app_activation_and_explicit_open_reuses_host() {
    let fixture = Fixture::new(Fake::default(), limits());
    let ready = fixture.ready().await;
    assert_eq!(ready.snapshot.state, State::Ready);
    assert_eq!(ready.snapshot.activation, Activation::NotRequested);
    for attempt in 1..=2 {
        let response = fixture
            .client
            .request(&Request::new(Control::Open {
                credentials: fixture.specification.credentials.clone(),
            }))
            .await
            .unwrap();
        let response = fixture
            .client
            .wait_result(response, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(response.snapshot.activation, Activation::Acknowledged);
        assert_eq!(response.snapshot.activation_attempt, attempt);
        assert_eq!(response.snapshot.address, ready.snapshot.address);
    }
    assert_eq!(fixture.driver.starts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.driver.opens.load(Ordering::SeqCst), 2);
    fixture.finish().await;
}

#[test]
fn one_hundred_direct_desktop_and_cli_starts_share_one_startup_transaction() {
    const CALLERS: usize = 100;
    let budgets = Limits::default();
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    );
    let endpoint = Endpoint::isolated(&namespace()).unwrap();
    let specification = spec(true);
    let cancellation = CancellationToken::new();
    let driver = Arc::new(Fake {
        open_initially: true,
        startup_delay: Duration::from_millis(120),
        open_delay: Duration::from_millis(200),
        ..Default::default()
    });
    let owners = Arc::new(AtomicUsize::new(0));
    let workers = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(CALLERS));
    let mut callers = Vec::new();
    for index in 0..CALLERS {
        let (runtime, endpoint, specification, cancellation, driver, owners, workers, barrier) = (
            runtime.clone(),
            endpoint.clone(),
            specification.clone(),
            cancellation.clone(),
            driver.clone(),
            owners.clone(),
            workers.clone(),
            barrier.clone(),
        );
        callers.push(std::thread::spawn(move || {
            barrier.wait();
            let start_owner = |control| {
                owners.fetch_add(1, Ordering::SeqCst);
                workers
                    .lock()
                    .unwrap()
                    .push(runtime.spawn(host::run_headless(
                        control,
                        specification.clone(),
                        driver.clone(),
                        cancellation.clone(),
                        budgets,
                    )));
            };
            if index % 2 == 0 {
                match host::claim_desktop(&runtime, endpoint.clone(), &specification, budgets)? {
                    DesktopClaim::Owned(control) => {
                        start_owner(control);
                        let client = Client::new(endpoint, budgets);
                        runtime.block_on(async {
                            let initial = client.status().await?;
                            client.wait_result(initial, budgets.attach).await
                        })
                    }
                    DesktopClaim::Reused(response) => Ok(response),
                }
            } else {
                host::start_with(endpoint.clone(), &specification, budgets, || {
                    let control = {
                        let _entered = runtime.enter();
                        OwnedControl::bind(endpoint)?
                    };
                    start_owner(control);
                    Ok(None)
                })
            }
        }));
    }
    let results: Vec<_> = callers
        .into_iter()
        .map(|caller| caller.join().unwrap().unwrap())
        .collect();
    cancellation.cancel();
    let tasks = std::mem::take(&mut *workers.lock().unwrap());
    for task in tasks {
        runtime.block_on(task).unwrap().unwrap();
    }
    assert_eq!(results.len(), CALLERS);
    assert_eq!(owners.load(Ordering::SeqCst), 1);
    assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
    assert_eq!(driver.opens.load(Ordering::SeqCst), 1);
    for result in &results {
        assert!(result.error.is_none(), "{result:?}");
        assert_eq!(result.snapshot.state, State::Ready);
        assert_eq!(result.snapshot.activation, Activation::Acknowledged);
        assert_eq!(result.process_id, results[0].process_id);
        assert_eq!(result.snapshot.address, results[0].snapshot.address);
    }
}

#[tokio::test]
async fn configuration_account_and_credential_changes_reject_reuse() {
    let fixture = Fixture::new(Fake::default(), limits());
    fixture.ready().await;
    let mut changed_options = options(false);
    changed_options.github_account = Some("another-fixture".into());
    let changed = StartupSpec::new(&changed_options, &Inputs::default()).unwrap();
    let response = fixture.client.request(&changed.request()).await.unwrap();
    assert_eq!(response.error.unwrap().code, "host_configuration_mismatch");
    let response = fixture
        .client
        .request(&Request::new(Control::Open {
            credentials: Inputs::new([], Some(SECRET.into())).credential_stamp(),
        }))
        .await
        .unwrap();
    assert_eq!(response.error.unwrap().code, "restart_required");
    assert_eq!(fixture.driver.opens.load(Ordering::SeqCst), 0);
    fixture.finish().await;
}

#[tokio::test]
async fn failed_backend_is_visible_and_diagnostics_never_echo_provider_text() {
    let fixture = Fixture::new(
        Fake {
            failed_backend: true,
            ..Default::default()
        },
        limits(),
    );
    let response = fixture.ready().await;
    assert_eq!(response.snapshot.state, State::Failed);
    assert!(response.snapshot.address.is_none());
    let encoded = String::from_utf8(host::encode_message(&response).unwrap()).unwrap();
    assert!(!encoded.contains(SECRET));
    assert_eq!(response.snapshot.diagnostics[0].code, "backend_failed");
    assert_eq!(fixture.driver.opens.load(Ordering::SeqCst), 0);
    fixture.finish().await;
}

#[tokio::test]
async fn startup_deadline_cancels_owned_startup_but_leaves_failure_controllable() {
    let fixture = Fixture::new(
        Fake {
            pending_start: true,
            ..Default::default()
        },
        Limits {
            startup: Duration::from_millis(60),
            ..limits()
        },
    );
    let response = fixture.ready().await;
    assert_eq!(response.snapshot.state, State::Failed);
    assert_eq!(response.snapshot.diagnostics[0].code, "startup_deadline");
    assert_eq!(fixture.driver.drains.load(Ordering::SeqCst), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn activation_timeout_is_uncertain_and_never_retried_by_start_or_open() {
    let fixture = Fixture::new(
        Fake {
            open_initially: true,
            pending_open: true,
            ..Default::default()
        },
        Limits {
            activation: Duration::from_millis(50),
            ..limits()
        },
    );
    let response = fixture.ready().await;
    assert_eq!(response.snapshot.state, State::Ready);
    assert_eq!(response.snapshot.activation, Activation::Uncertain);
    let response = fixture
        .client
        .request(&fixture.specification.request())
        .await
        .unwrap();
    assert_eq!(response.error.unwrap().code, "activation_uncertain");
    let response = fixture
        .client
        .request(&Request::new(Control::Open {
            credentials: fixture.specification.credentials.clone(),
        }))
        .await
        .unwrap();
    assert_eq!(response.error.unwrap().code, "activation_uncertain");
    assert_eq!(fixture.driver.opens.load(Ordering::SeqCst), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn failed_activation_keeps_ready_listener_and_is_not_automatically_retried() {
    let fixture = Fixture::new(
        Fake {
            open_initially: true,
            failed_open: true,
            ..Default::default()
        },
        limits(),
    );
    let response = fixture.ready().await;
    assert_eq!(response.snapshot.state, State::Ready);
    assert_eq!(response.snapshot.activation, Activation::Failed);
    let response = fixture
        .client
        .request(&fixture.specification.request())
        .await
        .unwrap();
    assert_eq!(response.error.as_ref().unwrap().code, "activation_failed");
    assert_eq!(fixture.driver.opens.load(Ordering::SeqCst), 1);
    assert!(
        !String::from_utf8(host::encode_message(&response).unwrap())
            .unwrap()
            .contains(SECRET)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn stalled_control_and_malformed_frames_are_bounded() {
    let endpoint = Endpoint::isolated(&namespace()).unwrap();
    let stalled = OwnedControl::bind(endpoint.clone()).unwrap();
    let client = Client::new(
        endpoint,
        Limits {
            request: Duration::from_millis(70),
            ..limits()
        },
    );
    let start = Instant::now();
    assert_eq!(
        client.status().await.unwrap_err().code,
        "host_control_timeout"
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(stalled);

    let fixture = Fixture::new(Fake::default(), limits());
    fixture.ready().await;
    let mut pipe = ClientOptions::new()
        .open(fixture.endpoint.pipe_name())
        .unwrap();
    host::write_message(
        &mut pipe,
        &json!({"version":99,"command":{"action":"status"}}),
    )
    .await
    .unwrap();
    let response = host::decode_response(&host::read_message(&mut pipe).await.unwrap()).unwrap();
    assert_eq!(response.error.unwrap().code, "host_protocol_version");
    drop(pipe);
    let mut slow = ClientOptions::new()
        .open(fixture.endpoint.pipe_name())
        .unwrap();
    slow.write_u32_le(300).await.unwrap();
    slow.write_all(b"{").await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), host::read_message(&mut slow))
        .await
        .expect("stalled pipe did not close within its deadline")
        .unwrap_err();
    assert_eq!(closed.code, "host_control_closed");
    assert_eq!(
        fixture.client.status().await.unwrap().snapshot.state,
        State::Ready
    );
    drop(slow);
    fixture.finish().await;
}

#[tokio::test]
async fn quit_acknowledges_then_drains_and_releases_only_the_owned_listener() {
    let fixture = Fixture::new(
        Fake {
            drain_delay: Duration::from_millis(160),
            ..Default::default()
        },
        limits(),
    );
    let address = fixture.ready().await.snapshot.address.unwrap();
    let other = listener::bind_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
    let response = fixture
        .client
        .request(&Request::new(Control::Stop))
        .await
        .unwrap();
    assert_eq!(response.snapshot.state, State::Stopping);
    assert!(listener::bind_loopback(address).is_err());
    assert_eq!(
        fixture.client.status().await.unwrap().snapshot.state,
        State::Stopping
    );
    let driver = fixture.driver.clone();
    fixture.finish().await;
    assert_eq!(driver.drains.load(Ordering::SeqCst), 1);
    assert!(listener::bind_loopback(address).is_ok());
    assert!(listener::bind_loopback(other.local_addr().unwrap()).is_err());
}

#[tokio::test]
async fn tray_actions_use_the_same_owned_lifecycle_without_real_ui() {
    let fixture = Fixture::new(Fake::default(), limits());
    fixture.ready().await;
    let handle = fixture.driver.host.lock().unwrap().clone().unwrap();
    assert_eq!(
        handle.route_tray(TrayAction::Status).snapshot.state,
        State::Ready
    );
    let response = handle.route_tray(TrayAction::Open);
    let response = fixture
        .client
        .wait_result(response, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(response.snapshot.activation, Activation::Acknowledged);
    assert_eq!(
        handle.route_tray(TrayAction::Quit).snapshot.state,
        State::Stopping
    );
    fixture.finish().await;
}

#[tokio::test]
async fn diagnostic_history_is_capped_and_redacted_across_explicit_retries() {
    let fixture = Fixture::new(
        Fake {
            failed_open: true,
            ..Default::default()
        },
        limits(),
    );
    fixture.ready().await;
    for _ in 0..11 {
        let response = fixture
            .client
            .request(&Request::new(Control::Open {
                credentials: fixture.specification.credentials.clone(),
            }))
            .await
            .unwrap();
        fixture
            .client
            .wait_result(response, Duration::from_secs(1))
            .await
            .unwrap();
    }
    let response = fixture.client.status().await.unwrap();
    assert_eq!(response.snapshot.diagnostics.len(), 8);
    assert_eq!(response.snapshot.activation_attempt, 11);
    let message = host::encode_message(&response).unwrap();
    assert!(message.len() < MAX_MESSAGE_BYTES);
    assert!(!String::from_utf8(message).unwrap().contains(SECRET));
    fixture.finish().await;
}

#[tokio::test]
async fn exceeded_drain_deadline_aborts_only_the_owned_backend() {
    let mut fixture = Fixture::new(
        Fake {
            drain_delay: Duration::from_secs(3),
            ..Default::default()
        },
        Limits {
            drain: Duration::from_millis(60),
            ..limits()
        },
    );
    let address = fixture.ready().await.snapshot.address.unwrap();
    let stopped = fixture
        .client
        .request(&Request::new(Control::Stop))
        .await
        .unwrap();
    assert_eq!(stopped.snapshot.state, State::Stopping);
    let result = tokio::time::timeout(Duration::from_secs(2), fixture.task.take().unwrap())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(result.code, "shutdown_deadline");
    assert!(listener::bind_loopback(address).is_ok());
    assert_eq!(
        fixture.client.status().await.unwrap_err().code,
        "host_not_running"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_start_transaction_does_not_spawn_another_host() {
    let fixture = Fixture::new(Fake::default(), limits());
    fixture.ready().await;
    let endpoint = fixture.endpoint.clone();
    let mut options = options(false);
    options.github_account = Some("other-fixture".into());
    let specification = StartupSpec::new(&options, &Inputs::default()).unwrap();
    let response = tokio::task::spawn_blocking(move || {
        host::start_with(endpoint, &specification, limits(), || {
            panic!("an incompatible running host must not spawn a replacement")
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.error.unwrap().code, "host_configuration_mismatch");
    assert_eq!(fixture.driver.starts.load(Ordering::SeqCst), 1);
    fixture.finish().await;
}

struct KernelHandle(HANDLE);
impl Drop for KernelHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[tokio::test]
async fn mismatched_server_image_is_rejected_before_any_command_is_written() {
    let endpoint = Endpoint::isolated(&namespace()).unwrap();
    let root = tempfile::Builder::new()
        .prefix("host-peer-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let ready = root.path().join("ready");
    let read = root.path().join("read");
    let client_done = root.path().join("client-done");
    let stderr = root.path().join("producer-stderr");
    let script = r#"
$ErrorActionPreference = 'Stop'
$p = $null
try {
    $p = [System.IO.Pipes.NamedPipeServerStream]::new(
        $env:HOST_TEST_PIPE, [System.IO.Pipes.PipeDirection]::InOut, 1,
        [System.IO.Pipes.PipeTransmissionMode]::Byte,
        [System.IO.Pipes.PipeOptions]::Asynchronous)
    $connected = $p.WaitForConnectionAsync()
    [System.IO.File]::WriteAllText($env:HOST_TEST_READY, 'ready')
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    while (-not [System.IO.File]::Exists($env:HOST_TEST_CLIENT_DONE)) {
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Client completion was not published.' }
        Start-Sleep -Milliseconds 10
    }
    if (-not $connected.Wait(3000)) { throw 'Pipe connection did not complete.' }
    $connected.GetAwaiter().GetResult()
    $n = $p.ReadByte()
    $p.Dispose()
    $p = $null
    $pending = $env:HOST_TEST_READ + '.pending'
    [System.IO.File]::WriteAllText(
        $pending, $n.ToString([System.Globalization.CultureInfo]::InvariantCulture))
    [System.IO.File]::Move($pending, $env:HOST_TEST_READ)
} catch {
    [Console]::Error.WriteLine($_.Exception.ToString())
    exit 1
} finally {
    if ($null -ne $p) { $p.Dispose() }
}
"#;
    let mut child = OwnedChild(
        Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env(
                "HOST_TEST_PIPE",
                endpoint.pipe_name().strip_prefix(r"\\.\pipe\").unwrap(),
            )
            .env("HOST_TEST_READY", &ready)
            .env("HOST_TEST_READ", &read)
            .env("HOST_TEST_CLIENT_DONE", &client_done)
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    wait_file(&ready, Duration::from_secs(8)).await;
    let result = Client::new(endpoint, limits())
        .request(&Request::new(Control::Stop))
        .await;
    // Force producer consumption after client rejection/closure, rather than relying on timing.
    std::fs::write(client_done, b"closed").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        child
            .0
            .try_wait()
            .unwrap()
            .is_some_and(|status| status.success()),
        "producer did not complete successfully: {}",
        std::fs::read_to_string(&stderr).unwrap_or_default(),
    );
    // File creation can precede WriteAllText completing; process exit seals the observation.
    assert_eq!(std::fs::read_to_string(read).unwrap(), "-1");
    assert_eq!(result.unwrap_err().code, "host_owner_mismatch");
}

async fn wait_file(path: &std::path::Path, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !path.is_file() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(path.is_file(), "fixture did not publish {}", path.display());
}

// Native job fixtures use only handles created here, never a system/user process lookup.
#[repr(C)]
#[derive(Default)]
struct BasicLimits {
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
struct ExtendedLimits {
    basic: BasicLimits,
    io: IO_COUNTERS,
    process_memory: usize,
    job_memory: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const SECURITY_ATTRIBUTES, name: *const u16) -> HANDLE;
    fn SetInformationJobObject(
        job: HANDLE,
        class: i32,
        information: *const c_void,
        length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: HANDLE, process: HANDLE) -> i32;
    fn IsProcessInJob(process: HANDLE, job: HANDLE, result: *mut i32) -> i32;
}

fn fixture_job(breakaway: bool) -> KernelHandle {
    let job = KernelHandle(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) });
    assert!(!job.0.is_null());
    let information = ExtendedLimits {
        basic: BasicLimits {
            flags: 0x2000 | if breakaway { 0x800 } else { 0 },
            ..Default::default()
        },
        ..Default::default()
    };
    assert_ne!(
        unsafe {
            SetInformationJobObject(
                job.0,
                9,
                (&information as *const ExtendedLimits).cast(),
                std::mem::size_of::<ExtendedLimits>() as u32,
            )
        },
        0
    );
    job
}

fn fixture_args(test: &str) -> Vec<OsString> {
    ["--ignored", "--exact", test, "--nocapture"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn publish_fixture(path: &std::path::Path, value: &serde_json::Value) {
    let pending = path.with_extension("pending");
    std::fs::write(&pending, serde_json::to_vec(value).unwrap()).unwrap();
    std::fs::rename(pending, path).unwrap();
}

#[test]
#[ignore = "internal direct/paired-role fixture; synthetic inputs/backend only, no UI"]
fn fixture_desktop_role_process() {
    assert_eq!(std::env::var("ADAPTER_HOST_FIXTURE").as_deref(), Ok("1"));
    if let Some(gate) = std::env::var_os("ADAPTER_HOST_GATE") {
        let gate = PathBuf::from(gate);
        let deadline = Instant::now() + Duration::from_secs(8);
        while !gate.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gate.is_file());
    }
    let report = PathBuf::from(std::env::var_os("ADAPTER_HOST_REPORT").unwrap());
    let action = std::env::var("ADAPTER_HOST_ROLE_ACTION").unwrap();
    let endpoint = Endpoint::isolated(&std::env::var("ADAPTER_HOST_NAMESPACE").unwrap()).unwrap();
    let specification = spec(true);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let result = if action == "desktop" {
        match host::claim_desktop(&runtime, endpoint.clone(), &specification, limits()) {
            Ok(DesktopClaim::Owned(control)) => {
                let driver = Arc::new(Fake {
                    open_initially: true,
                    ..Default::default()
                });
                let cancellation = CancellationToken::new();
                let lifetime = cancellation.clone();
                let deadline = runtime.spawn(async move {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    lifetime.cancel();
                });
                let server = runtime.spawn(host::run_headless(
                    control,
                    specification,
                    driver.clone(),
                    cancellation,
                    limits(),
                ));
                let ready = runtime
                    .block_on(async {
                        let client = Client::new(endpoint, limits());
                        let response = client.status().await?;
                        client.wait_result(response, limits().attach).await
                    })
                    .unwrap();
                publish_fixture(&report, &json!({"claim":"owned", "response":ready}));
                runtime.block_on(server).unwrap().unwrap();
                deadline.abort();
                runtime.block_on(async {
                    let _ = deadline.await;
                });
                publish_fixture(
                    &report.with_extension("finished"),
                    &json!({
                        "starts": driver.starts.load(Ordering::SeqCst),
                        "opens": driver.opens.load(Ordering::SeqCst),
                        "drains": driver.drains.load(Ordering::SeqCst),
                    }),
                );
                runtime.shutdown_timeout(Duration::from_secs(1));
                return;
            }
            Ok(DesktopClaim::Reused(response)) => {
                Ok(json!({"claim":"reused", "response":response}))
            }
            Err(error) => Err(error),
        }
    } else if action == "cli-start" {
        host::start_with(endpoint, &specification, limits(), || {
            Err(AdapterError::new(
                500,
                "fixture_spawn_forbidden",
                "An existing paired host must be reused.",
            ))
        })
        .map(|response| json!({"claim":"cli-reused", "response":response}))
    } else {
        runtime.block_on(async {
            let client = Client::new(endpoint, limits());
            let response = match action.as_str() {
                "status" => client.status().await?,
                "stop" => client.stop_and_wait().await?,
                "open" => {
                    let response = client
                        .request(&Request::new(Control::Open {
                            credentials: specification.credentials,
                        }))
                        .await?;
                    client.wait_result(response, limits().attach).await?
                }
                _ => return Err(AdapterError::invalid("Unknown fixture action.")),
            };
            Ok(json!({"response":response}))
        })
    };
    publish_fixture(
        &report,
        &match result {
            Ok(value) => value,
            Err(error) => json!({"error":error.code}),
        },
    );
    runtime.shutdown_timeout(Duration::from_secs(1));
}

fn desktop_fixture_child(
    executable: &std::path::Path,
    namespace: &str,
    report: &std::path::Path,
    action: &str,
    gate: Option<&std::path::Path>,
) -> OwnedChild {
    let mut command = Command::new(executable);
    command
        .args(fixture_args("fixture_desktop_role_process"))
        .env("ADAPTER_HOST_FIXTURE", "1")
        .env("ADAPTER_HOST_ROLE_ACTION", action)
        .env("ADAPTER_HOST_NAMESPACE", namespace)
        .env("ADAPTER_HOST_REPORT", report)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(gate) = gate {
        command.env("ADAPTER_HOST_GATE", gate);
    }
    OwnedChild(command.spawn().unwrap())
}

async fn read_fixture_report(path: &std::path::Path) -> serde_json::Value {
    wait_file(path, Duration::from_secs(8)).await;
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

async fn completed_role(
    executable: &std::path::Path,
    namespace: &str,
    report: &std::path::Path,
    action: &str,
) -> serde_json::Value {
    let mut child = desktop_fixture_child(executable, namespace, report, action, None);
    let value = read_fixture_report(report).await;
    assert_eq!(
        unsafe { WaitForSingleObject(child.0.as_raw_handle(), 2000) },
        WAIT_OBJECT_0
    );
    assert!(child.0.try_wait().unwrap().unwrap().success());
    value
}

#[tokio::test]
async fn direct_desktop_claim_works_in_an_owned_prohibited_breakaway_job() {
    let root = tempfile::Builder::new()
        .prefix("host-job-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let report = root.path().join("owner.json");
    let gate = root.path().join("gate");
    let namespace = namespace();
    let endpoint = Endpoint::isolated(&namespace).unwrap();
    let job = fixture_job(false);
    let mut owner = desktop_fixture_child(
        &std::env::current_exe().unwrap(),
        &namespace,
        &report,
        "desktop",
        Some(&gate),
    );
    assert_ne!(
        unsafe { AssignProcessToJobObject(job.0, owner.0.as_raw_handle()) },
        0
    );
    let mut member = 0;
    assert_ne!(
        unsafe { IsProcessInJob(owner.0.as_raw_handle(), job.0, &mut member) },
        0
    );
    assert_eq!(member, 1);
    std::fs::write(gate, b"assigned").unwrap();
    let ready = read_fixture_report(&report).await;
    assert_eq!(ready["claim"], "owned", "{ready}");
    assert_eq!(ready["response"]["snapshot"]["state"], "ready");
    assert_eq!(ready["response"]["process_id"], owner.0.id());
    assert_eq!(ready["response"]["snapshot"]["activation"], "acknowledged");
    let stopped = Client::new(endpoint, limits())
        .stop_and_wait()
        .await
        .unwrap();
    assert!(stopped.exited);
    assert!(owner.0.try_wait().unwrap().unwrap().success());
    let finished = read_fixture_report(&report.with_extension("finished")).await;
    assert_eq!(finished["starts"], 1);
    assert_eq!(finished["drains"], 1);
}

#[tokio::test]
async fn paired_role_processes_reuse_gui_owner_and_reject_wrong_images_and_directories() {
    let root = tempfile::Builder::new()
        .prefix("host-peer-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let original = std::env::current_exe().unwrap();
    let gui = root.path().join("github-adapter-host.exe");
    let cli = root.path().join("github-adapter.exe");
    let lookalike = root.path().join("github-adapter-host-copy.exe");
    let other = root.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let elsewhere = other.join("github-adapter.exe");
    for target in [&gui, &cli, &lookalike, &elsewhere] {
        std::fs::copy(&original, target).unwrap();
    }
    let namespace = namespace();
    let owner_report = root.path().join("owner.json");
    let mut owner = desktop_fixture_child(&gui, &namespace, &owner_report, "desktop", None);
    let ready = read_fixture_report(&owner_report).await;
    assert_eq!(ready["claim"], "owned", "{ready}");
    let pid = owner.0.id();
    let again = completed_role(&gui, &namespace, &root.path().join("again.json"), "desktop").await;
    assert_eq!(again["claim"], "reused", "{again}");
    assert_eq!(again["response"]["process_id"], pid);
    let cli_start = completed_role(
        &cli,
        &namespace,
        &root.path().join("cli-start.json"),
        "cli-start",
    )
    .await;
    assert_eq!(cli_start["claim"], "cli-reused", "{cli_start}");
    assert_eq!(cli_start["response"]["process_id"], pid);
    for (index, wrong) in [&lookalike, &elsewhere].into_iter().enumerate() {
        let result = completed_role(
            wrong,
            &namespace,
            &root.path().join(format!("wrong-{index}.json")),
            "status",
        )
        .await;
        assert_eq!(result["error"], "host_owner_mismatch", "{result}");
    }
    let opened = completed_role(&cli, &namespace, &root.path().join("open.json"), "open").await;
    assert_eq!(
        opened["response"]["snapshot"]["activation"], "acknowledged",
        "{opened}"
    );
    assert_eq!(opened["response"]["process_id"], pid);
    let stopped = completed_role(&cli, &namespace, &root.path().join("stop.json"), "stop").await;
    assert_eq!(stopped["response"]["exited"], true, "{stopped}");
    assert!(owner.0.try_wait().unwrap().unwrap().success());
    let finished = read_fixture_report(&owner_report.with_extension("finished")).await;
    assert_eq!(finished["starts"], 1);
    assert_eq!(finished["drains"], 1);
}

#[test]
#[ignore = "internal synthetic child entry; invoked only by owned process contracts"]
fn fixture_host_process() {
    assert_eq!(std::env::var("ADAPTER_HOST_FIXTURE").as_deref(), Ok("1"));
    host::verify_independent_process().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let endpoint =
            Endpoint::isolated(&std::env::var("ADAPTER_HOST_NAMESPACE").unwrap()).unwrap();
        let control = OwnedControl::bind(endpoint).unwrap();
        let cancellation = CancellationToken::new();
        let lifetime = cancellation.clone();
        let deadline = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(25)).await;
            lifetime.cancel();
        });
        host::run_headless(
            control,
            spec(false),
            Arc::new(Fake::default()),
            cancellation,
            limits(),
        )
        .await
        .unwrap();
        deadline.abort();
        let _ = deadline.await;
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
}

#[test]
#[ignore = "internal synthetic parent entry; invoked only by owned process contracts"]
fn fixture_launcher_process() {
    assert_eq!(std::env::var("ADAPTER_HOST_FIXTURE").as_deref(), Ok("1"));
    let gate = PathBuf::from(std::env::var_os("ADAPTER_HOST_GATE").unwrap());
    let report = PathBuf::from(std::env::var_os("ADAPTER_HOST_REPORT").unwrap());
    let deadline = Instant::now() + Duration::from_secs(12);
    while !gate.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(gate.is_file());
    if std::env::var("ADAPTER_HOST_DIAGNOSTIC_CASE").as_deref() == Ok("1") {
        let output = Command::new(env!("CARGO_BIN_EXE_github-adapter"))
            .arg("--diagnose-host-jobs")
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.len() <= MAX_MESSAGE_BYTES);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        std::fs::write(&report, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::write(report.with_extension("ready"), b"written").unwrap();
        std::thread::sleep(Duration::from_secs(25));
        return;
    }
    let executable = std::env::current_exe().unwrap();
    let endpoint = Endpoint::isolated(&std::env::var("ADAPTER_HOST_NAMESPACE").unwrap()).unwrap();
    let result = host::start_with(endpoint, &spec(false), limits(), || {
        host::spawn_detached(&executable, &fixture_args("fixture_host_process")).map(Some)
    });
    let report_value = match result {
        Ok(response) => json!({"response":response}),
        Err(error) => json!({"error":error.code, "message":error.message}),
    };
    std::fs::write(report, serde_json::to_vec(&report_value).unwrap()).unwrap();
    std::thread::sleep(Duration::from_secs(25));
}

async fn job_contract(breakaway: bool, require_survival: bool) {
    let root = tempfile::Builder::new()
        .prefix("host-job-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let gate = root.path().join("gate");
    let report = root.path().join("report.json");
    let namespace = namespace();
    let job = fixture_job(breakaway);
    let mut launcher = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(fixture_args("fixture_launcher_process"))
            .env("ADAPTER_HOST_FIXTURE", "1")
            .env("ADAPTER_HOST_NAMESPACE", &namespace)
            .env("ADAPTER_HOST_GATE", &gate)
            .env("ADAPTER_HOST_REPORT", &report)
            .creation_flags(
                CREATE_NEW_PROCESS_GROUP
                    | CREATE_NO_WINDOW
                    | if require_survival {
                        CREATE_BREAKAWAY_FROM_JOB
                    } else {
                        0
                    },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert_ne!(
        unsafe { AssignProcessToJobObject(job.0, launcher.0.as_raw_handle()) },
        0
    );
    std::fs::write(&gate, b"assigned").unwrap();
    wait_file(&report, Duration::from_secs(10)).await;
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    let client = Client::new(Endpoint::isolated(&namespace).unwrap(), limits());
    if !breakaway {
        drop(job);
        assert_eq!(report["error"], "host_detach_failed", "{report}");
        assert_eq!(client.status().await.unwrap_err().code, "host_not_running");
        return;
    }
    if !require_survival && report["error"] == "host_job_unverified" {
        drop(job);
        assert_eq!(client.status().await.unwrap_err().code, "host_not_running");
        return;
    }
    assert!(report.get("error").is_none(), "{report}");
    let before = client.status().await.unwrap();
    let host_pid = before.process_id;
    assert_ne!(host_pid, launcher.0.id());
    assert_eq!(before.snapshot.state, State::Ready);
    drop(job);
    assert_eq!(
        unsafe { WaitForSingleObject(launcher.0.as_raw_handle(), 3000) },
        WAIT_OBJECT_0
    );
    assert!(launcher.0.try_wait().unwrap().is_some());
    let after = client.status().await.unwrap();
    assert_eq!(after.process_id, host_pid);
    let mut http = TcpStream::connect(after.snapshot.address.unwrap())
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), http.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.ends_with(b"synthetic"));
    let stopped = client.stop_and_wait().await.unwrap();
    assert!(stopped.exited);
    assert_eq!(stopped.process_id, host_pid);
    assert_eq!(client.status().await.unwrap_err().code, "host_not_running");
}

#[tokio::test]
#[ignore = "opt-in from a terminal allowing outer-job breakaway; never bypass a job policy"]
async fn detached_synthetic_host_survives_its_launcher_job_termination() {
    job_contract(true, true).await;
}

#[tokio::test]
async fn enclosing_job_membership_is_verified_or_explicitly_refused() {
    job_contract(true, false).await;
}

#[tokio::test]
async fn prohibited_job_breakaway_is_reported_without_an_attached_fallback() {
    job_contract(false, false).await;
}

fn job_metadata_probe(jobs: &[HANDLE]) -> OwnedChild {
    let child = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_github-adapter"))
            .args(["--host-job-probe", &std::process::id().to_string()])
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for job in jobs {
        assert_ne!(
            unsafe { AssignProcessToJobObject(*job, child.0.as_raw_handle()) },
            0
        );
    }
    child
}

fn read_job_observation(child: &mut OwnedChild) -> serde_json::Value {
    use std::io::{Read, Write};
    child.0.stdin.as_mut().unwrap().write_all(b"R").unwrap();
    let output = child.0.stdout.as_mut().unwrap();
    let mut length = [0; 4];
    output.read_exact(&mut length).unwrap();
    let length = u32::from_le_bytes(length) as usize;
    assert!(length > 0 && length <= MAX_MESSAGE_BYTES);
    let mut bytes = vec![0; length];
    output.read_exact(&mut bytes).unwrap();
    let observation: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(observation["version"], 1);
    assert_eq!(observation["process_id"], child.0.id());
    observation
}

#[test]
fn job_membership_alone_does_not_mean_close_kills_the_process() {
    let job = KernelHandle(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) });
    assert!(!job.0.is_null());
    let mut child = job_metadata_probe(&[job.0]);
    let first = read_job_observation(&mut child);
    assert_eq!(first["any_job"], true);
    assert_eq!(first["immediate_limits"]["raw"], 0);
    assert_eq!(first["immediate_limits"]["kill_on_close"], false);
    drop(job);
    // A fresh round trip, not merely a liveness timeout, proves this specific close was harmless.
    let second = read_job_observation(&mut child);
    assert_eq!(second["any_job"], true);
    assert_eq!(second["immediate_limits"]["raw"], 0);
    assert_eq!(
        unsafe { WaitForSingleObject(child.0.as_raw_handle(), 1000) },
        WAIT_OBJECT_0
    );
    assert!(child.0.try_wait().unwrap().unwrap().success());
}

#[test]
fn immediate_job_without_limits_can_hide_a_kill_on_close_ancestor() {
    let outer = fixture_job(false);
    let inner = KernelHandle(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) });
    assert!(!inner.0.is_null());
    let mut child = job_metadata_probe(&[outer.0, inner.0]);
    let observation = read_job_observation(&mut child);
    assert_eq!(observation["immediate_limits"]["raw"], 0);
    assert_eq!(observation["immediate_limits"]["kill_on_close"], false);
    let mut in_outer = 0;
    assert_ne!(
        unsafe { IsProcessInJob(child.0.as_raw_handle(), outer.0, &mut in_outer) },
        0
    );
    assert_eq!(in_outer, 1);
    drop(outer);
    assert_eq!(
        unsafe { WaitForSingleObject(child.0.as_raw_handle(), 2000) },
        WAIT_OBJECT_0
    );
    assert!(child.0.try_wait().unwrap().is_some());
}

#[test]
fn job_diagnostic_is_bounded_non_sensitive_and_never_claims_provider_readiness() {
    let output = Command::new(env!("CARGO_BIN_EXE_github-adapter"))
        .arg("--diagnose-host-jobs")
        .env(cli::TOKEN_ENV, SECRET)
        .env(cli::LEGACY_TOKEN_ENV, "deliberate-fixture-conflict")
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() <= MAX_MESSAGE_BYTES);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["diagnostic"], "windows_job_inheritance");
    assert_eq!(report["provider_started"], false);
    assert_eq!(report["independence_proved"], false);
    assert_eq!(report["ancestor_jobs_observed"], false);
    assert_eq!(report["job_handle_owners_observed"], false);
    if report.get("create_process_error").is_some() {
        assert_eq!(report["conclusion"], "create_process_refused");
    } else {
        assert_eq!(report["child_exited"], true);
        assert_eq!(report["child_exit_code"], 0);
        assert_eq!(report["forced_fixture_cleanup"], false);
        assert_eq!(report["child"]["console_attached"], false);
        assert!(!report["child"].is_null(), "{report}");
    }
}

#[tokio::test]
async fn diagnostic_observes_successful_partial_breakaway_without_claiming_independence() {
    let root = tempfile::Builder::new()
        .prefix("host-job-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let gate = root.path().join("gate");
    let report = root.path().join("report.json");
    let job = fixture_job(true);
    let mut launcher = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(fixture_args("fixture_launcher_process"))
            .env("ADAPTER_HOST_FIXTURE", "1")
            .env("ADAPTER_HOST_DIAGNOSTIC_CASE", "1")
            .env("ADAPTER_HOST_GATE", &gate)
            .env("ADAPTER_HOST_REPORT", &report)
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert_ne!(
        unsafe { AssignProcessToJobObject(job.0, launcher.0.as_raw_handle()) },
        0
    );
    std::fs::write(gate, b"assigned").unwrap();
    wait_file(&report.with_extension("ready"), Duration::from_secs(10)).await;
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(
        report["parent_before"]["immediate_limits"]["breakaway_ok"],
        true
    );
    assert!(report.get("create_process_error").is_none(), "{report}");
    assert!(!report["child"].is_null(), "{report}");
    assert_eq!(report["child_live_during_observation"], true);
    assert_eq!(
        report["child_membership_query_error"],
        serde_json::Value::Null
    );
    assert_eq!(
        report["child_any_job_seen_by_parent"],
        report["child"]["any_job"]
    );
    assert_eq!(report["child_exited"], true);
    assert_eq!(report["child_exit_code"], 0);
    assert_eq!(report["forced_fixture_cleanup"], false);
    assert_eq!(report["independence_proved"], false);
    assert_eq!(
        report["parent_after"]["immediate_members"]["peer_present"],
        false
    );
    if report["child"]["any_job"] == true {
        assert!(matches!(
            report["conclusion"].as_str(),
            Some("shared_immediate_job_observed" | "unresolved_job_ancestry")
        ));
        if report["conclusion"] == "shared_immediate_job_observed" {
            assert_eq!(report["child"]["immediate_members"]["peer_present"], true);
        }
    } else {
        assert_eq!(report["conclusion"], "jobless_child_observed");
    }
    drop(job);
    assert_eq!(
        unsafe { WaitForSingleObject(launcher.0.as_raw_handle(), 2000) },
        WAIT_OBJECT_0
    );
    assert!(launcher.0.try_wait().unwrap().is_some());
}

#[test]
fn owned_lifetime_diagnostic_distinguishes_launcher_close_from_reapplied_1800_ancestor() {
    let output = Command::new(env!("CARGO_BIN_EXE_github-adapter"))
        .args(["--diagnose-host-jobs", "--owned-lifetime"])
        .env(cli::TOKEN_ENV, SECRET)
        .env(cli::LEGACY_TOKEN_ENV, "deliberate-fixture-conflict")
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() <= 32 * 1024);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let lifetime = &report["owned_lifetime"];
    let good = &lifetime["breakaway_case"];
    let denied = &lifetime["reapplied_1800_denied_ancestor_control"];
    assert_eq!(report["provider_started"], false);
    assert_eq!(report["independence_proved"], false);
    assert_eq!(lifetime["ambient_ancestry_proved"], false);
    assert_eq!(
        good["conclusion"], "owned_launcher_job_independence_observed",
        "{report}"
    );
    assert_eq!(
        good["launcher"]["launcher_before"]["immediate_limits"]["raw"],
        0x2800
    );
    assert_eq!(good["explicit_launcher_job_limits"]["raw"], 0x2800);
    assert_eq!(good["launcher_in_explicit_job"], true);
    assert_eq!(good["leaf_in_launcher_job_before_close"], false);
    assert_eq!(good["launcher_exited_after_owned_job_close"], true);
    assert_eq!(good["fresh_leaf_reply_after_close"], true);
    assert_eq!(good["leaf_exited"], true);
    assert_eq!(good["leaf_exit_code"], 0);
    assert_eq!(
        denied["conclusion"], "reapplied_1800_does_not_exclude_kill_ancestor",
        "{report}"
    );
    assert_eq!(
        denied["launcher"]["launcher_before"]["immediate_limits"]["raw"],
        0x1800
    );
    assert_eq!(denied["explicit_launcher_job_limits"]["raw"], 0x1800);
    assert_eq!(
        denied["leaf_before_close"]["immediate_limits"]["raw"],
        0x1800
    );
    assert_eq!(
        denied["leaf_before_close"]["immediate_members"]["peer_present"],
        true
    );
    assert_eq!(
        denied["explicit_launcher_job_members_after_reapply"]["peer_present"],
        true
    );
    assert_eq!(denied["leaf_in_denied_ancestor_before_close"], true);
    assert_eq!(denied["launcher_exited_after_owned_job_close"], true);
    assert_eq!(denied["fresh_leaf_reply_after_close"], false);
    assert_eq!(denied["leaf_exited"], true);
}
