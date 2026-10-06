#![cfg(windows)]
use adapter_app::config::{self, Client, ClientPaths, ConfigureOptions, on_demand as mode};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ORIGINAL: &str = "# Keep my normal setup\r\nmodel_provider = \"openai\"\r\nmodel = \"gpt-6-astra\"\r\nmodel_reasoning_effort = \"ultra\"\r\n\r\n[features]\r\nuser_preference = true\r\n";
const ENDPOINT: &str = "http://127.0.0.1:53127";

fn paths(root: &Path) -> ClientPaths {
    ClientPaths {
        codex_config: Some(root.join("config.toml")),
        claude_settings: None,
        backup_dir: root.join("backups"),
    }
}
fn options() -> ConfigureOptions {
    ConfigureOptions {
        model: Some("gpt-6-astra".into()),
        reasoning_effort: Some("max".into()),
        ..Default::default()
    }
}
fn prepare(paths: &ClientPaths) -> mode::Owner {
    mode::prepare(
        paths,
        ENDPOINT,
        &options(),
        std::process::id(),
        creation_time(),
    )
    .unwrap()
}
fn creation_time() -> u64 {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit = created;
    let mut kernel = created;
    let mut user = created;
    assert_ne!(
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        },
        0
    );
    (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
}
fn text(paths: &ClientPaths) -> String {
    fs::read_to_string(paths.codex_config.as_ref().unwrap()).unwrap()
}
fn fixture() -> (tempfile::TempDir, ClientPaths) {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    fs::write(paths.codex_config.as_ref().unwrap(), ORIGINAL).unwrap();
    (root, paths)
}
fn wait_for(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for automatic recovery"
        );
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[test]
fn clean_shutdown_restores_exact_normal_bytes_and_retires_mode() {
    let (_root, paths) = fixture();
    let owner = prepare(&paths);
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    assert!(text(&paths).contains(ENDPOINT));
    mode::restore(&owner).unwrap();
    assert_eq!(text(&paths), ORIGINAL);
    assert!(mode::load(&paths).unwrap().is_none());
    assert!(
        !mode::mode_paths(&paths)
            .backup_dir
            .join("manifest.json")
            .exists()
    );
}
#[test]
fn absent_original_configuration_is_absent_again_when_off() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let owner = prepare(&paths);
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    mode::restore(&owner).unwrap();
    assert!(!paths.codex_config.unwrap().exists());
}
#[test]
fn user_edits_survive_automatic_restore() {
    let (_root, paths) = fixture();
    let owner = prepare(&paths);
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    let updated = text(&paths).replace(
        "model_reasoning_effort = \"max\"",
        "model_reasoning_effort = \"high\"",
    ) + "new_user_flag = 42\n";
    fs::write(paths.codex_config.as_ref().unwrap(), updated).unwrap();
    mode::restore(&owner).unwrap();
    let result = text(&paths);
    assert!(!result.contains(ENDPOINT));
    assert!(result.contains("\"high\""));
    assert!(result.contains("new_user_flag = 42"));
}
#[test]
fn explicitly_changed_endpoint_is_not_overwritten() {
    let (_root, paths) = fixture();
    let owner = prepare(&paths);
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    fs::write(
        paths.codex_config.as_ref().unwrap(),
        text(&paths).replace(ENDPOINT, "https://selected.example/v1"),
    )
    .unwrap();
    mode::restore(&owner).unwrap();
    assert!(text(&paths).contains("https://selected.example/v1"));
}
#[test]
fn deleted_user_configuration_is_not_recreated() {
    let (_root, paths) = fixture();
    let owner = prepare(&paths);
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    fs::remove_file(paths.codex_config.as_ref().unwrap()).unwrap();
    mode::restore(&owner).unwrap();
    assert!(!paths.codex_config.unwrap().exists());
}
#[test]
fn old_guard_cannot_restore_a_new_activation() {
    let (_root, paths) = fixture();
    let old = prepare(&paths);
    mode::activate(&old, ENDPOINT, &options()).unwrap();
    mode::restore(&old).unwrap();
    let current = prepare(&paths);
    mode::activate(&current, ENDPOINT, &options()).unwrap();
    mode::restore(&old).unwrap();
    assert!(text(&paths).contains(ENDPOINT));
    assert_eq!(mode::load(&paths).unwrap().unwrap().id, current.id);
    mode::restore(&current).unwrap();
}
#[test]
fn legacy_persistent_routing_migrates_without_losing_user_edits_or_first_original() {
    let (_root, paths) = fixture();
    config::configure(&paths, ENDPOINT, &[Client::Codex], &options()).unwrap();
    let original = fs::read(paths.backup_dir.join("codex.original")).unwrap();
    let manifest = fs::read(paths.backup_dir.join("manifest.json")).unwrap();
    fs::write(
        paths.codex_config.as_ref().unwrap(),
        text(&paths) + "later_user_flag = true\n",
    )
    .unwrap();
    let owner = prepare(&paths);
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    mode::restore(&owner).unwrap();
    assert!(!text(&paths).contains(ENDPOINT));
    assert!(text(&paths).contains("\"ultra\""));
    assert!(text(&paths).contains("later_user_flag = true"));
    assert_eq!(
        fs::read(paths.backup_dir.join("codex.original")).unwrap(),
        original
    );
    assert_eq!(
        fs::read(paths.backup_dir.join("manifest.json")).unwrap(),
        manifest
    );
}
#[test]
fn abandoned_preparation_recovers_legacy_routing_too() {
    let (_root, paths) = fixture();
    config::configure(&paths, ENDPOINT, &[Client::Codex], &options()).unwrap();
    let owner = prepare(&paths);
    mode::restore(&owner).unwrap();
    assert!(!text(&paths).contains(ENDPOINT));
    assert!(text(&paths).contains("\"ultra\""));
}
#[test]
fn never_guess_a_normal_endpoint_when_no_verified_original_exists() {
    let (_root, paths) = fixture();
    fs::write(
        paths.codex_config.as_ref().unwrap(),
        format!("openai_base_url = \"{ENDPOINT}\"\n"),
    )
    .unwrap();
    let before = text(&paths);
    assert!(
        mode::prepare(
            &paths,
            ENDPOINT,
            &options(),
            std::process::id(),
            creation_time()
        )
        .is_err()
    );
    assert!(mode::load(&paths).unwrap().is_none());
    assert_eq!(text(&paths), before);
}
#[test]
fn changed_original_is_rejected_without_overwriting_current_settings() {
    let (_root, paths) = fixture();
    let owner = prepare(&paths);
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    let current = text(&paths);
    fs::write(
        mode::mode_paths(&paths).backup_dir.join("codex.original"),
        "model = \"tampered\"\n",
    )
    .unwrap();
    assert!(mode::restore(&owner).is_err());
    assert_eq!(text(&paths), current);
    assert!(mode::load(&paths).unwrap().is_some());
}
#[test]
fn pipe_eof_recovers_without_a_host_exit_handler() {
    let (_root, paths) = fixture();
    let owner = prepare(&paths);
    let mut command = Command::new(env!("CARGO_BIN_EXE_github-adapter"));
    command
        .args([
            "--mode-guardian",
            &owner.target,
            &owner.backup_dir,
            &owner.id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
    let mut guard = command.spawn().unwrap();
    let input = guard.stdin.take().unwrap();
    let mut line = String::new();
    BufReader::new(guard.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "READY\n");
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    drop(input);
    wait_for(|| {
        fs::read(paths.codex_config.as_ref().unwrap()).is_ok_and(|raw| raw == ORIGINAL.as_bytes())
    });
    assert!(guard.wait().unwrap().success());
    assert!(mode::load(&paths).unwrap().is_none());
}
#[test]
fn logon_recovery_does_not_touch_a_live_owner() {
    let (_root, paths) = fixture();
    let owner = prepare(&paths);
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_github-adapter"))
        .arg("--recover-mode")
        .arg(mode::mode_paths(&paths).backup_dir)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(text(&paths).contains(ENDPOINT));
    mode::restore(&owner).unwrap();
}
#[test]
fn logon_recovery_restores_a_dead_owner() {
    let (_root, paths) = fixture();
    let owner = mode::prepare(&paths, ENDPOINT, &options(), u32::MAX, 1).unwrap();
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_github-adapter-host"))
        .arg("--logon-recovery")
        .arg(mode::mode_paths(&paths).backup_dir)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(text(&paths), ORIGINAL);
    assert!(mode::load(&paths).unwrap().is_none());
}

#[test]
#[ignore = "owned crash fixture; invoked by the parent-exit contract only"]
fn guardian_crash_parent_fixture() {
    let root = std::path::PathBuf::from(
        std::env::var_os("GITHUB_ADAPTER_MODE_FIXTURE_ROOT").expect("isolated fixture directory"),
    );
    assert!(
        root.is_absolute()
            && root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("github-adapter-mode-crash-")
    );
    let paths = paths(&root);
    let owner = prepare(&paths);
    let mut command = Command::new(env!("CARGO_BIN_EXE_github-adapter"));
    command
        .args([
            "--mode-guardian",
            &owner.target,
            &owner.backup_dir,
            &owner.id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
    let mut guard = command.spawn().unwrap();
    let _input = guard.stdin.take().unwrap();
    let mut line = String::new();
    BufReader::new(guard.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "READY\n");
    mode::activate(&owner, ENDPOINT, &options()).unwrap();
    fs::write(root.join("ready"), guard.id().to_string()).unwrap();
    std::thread::spawn(move || {
        let _ = guard.wait();
    });
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn forced_parent_termination_restores_normal_without_running_parent_cleanup() {
    let root = tempfile::Builder::new()
        .prefix("github-adapter-mode-crash-")
        .tempdir()
        .unwrap();
    let paths = paths(root.path());
    fs::write(paths.codex_config.as_ref().unwrap(), ORIGINAL).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "guardian_crash_parent_fixture",
            "--nocapture",
        ])
        .env("GITHUB_ADAPTER_MODE_FIXTURE_ROOT", root.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
    let mut parent = command.spawn().unwrap();
    wait_for(|| root.path().join("ready").is_file());
    assert!(text(&paths).contains(ENDPOINT));
    parent.kill().unwrap();
    parent.wait().unwrap();
    wait_for(|| {
        fs::read(paths.codex_config.as_ref().unwrap()).is_ok_and(|raw| raw == ORIGINAL.as_bytes())
            && mode::load(&paths).is_ok_and(|owner| owner.is_none())
    });
}

#[test]
fn normal_routing_recovery_never_displaces_a_live_listener() {
    let (_root, paths) = fixture();
    let listener = adapter_app::listener::bind_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = format!("http://{address}");
    config::configure(&paths, &endpoint, &[Client::Codex], &options()).unwrap();
    let before = text(&paths);
    assert!(
        adapter_app::on_demand::restore_normal_if_stopped(paths.clone(), address.port()).is_err()
    );
    assert_eq!(text(&paths), before);
    assert!(mode::load(&paths).unwrap().is_none());
}

#[test]
fn an_already_normal_configuration_needs_no_companion_or_registration() {
    let (_root, paths) = fixture();
    adapter_app::on_demand::restore_normal_if_stopped(paths.clone(), 5001).unwrap();
    assert_eq!(text(&paths), ORIGINAL);
    assert!(!mode::mode_paths(&paths).backup_dir.exists());
}

#[test]
fn unedited_legacy_configuration_restores_exact_original_crlf_bytes() {
    let (_root, paths) = fixture();
    config::configure(&paths, ENDPOINT, &[Client::Codex], &options()).unwrap();
    let manifest = fs::read(paths.backup_dir.join("manifest.json")).unwrap();
    mode::restore_legacy_route(&paths, ENDPOINT).unwrap();
    assert_eq!(
        fs::read(paths.codex_config.as_ref().unwrap()).unwrap(),
        ORIGINAL.as_bytes()
    );
    assert_eq!(
        fs::read(paths.backup_dir.join("manifest.json")).unwrap(),
        manifest
    );
    assert_eq!(
        fs::read(paths.backup_dir.join("codex.original")).unwrap(),
        ORIGINAL.as_bytes()
    );
}

#[test]
fn unedited_legacy_created_configuration_is_absent_after_restore() {
    let (_root, paths) = fixture();
    fs::remove_file(paths.codex_config.as_ref().unwrap()).unwrap();
    config::configure(&paths, ENDPOINT, &[Client::Codex], &options()).unwrap();
    mode::restore_legacy_route(&paths, ENDPOINT).unwrap();
    assert!(!paths.codex_config.as_ref().unwrap().exists());
}
