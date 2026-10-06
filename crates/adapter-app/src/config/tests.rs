use super::*;
use std::cell::RefCell;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

enum Action {
    Error,
    Exit,
    ExternalEdit(PathBuf, Vec<u8>),
}

thread_local! {
    static FAULT: RefCell<Option<(String, Action)>> = const { RefCell::new(None) };
}

fn inject(label: &str, action: Action) {
    FAULT.with(|fault| *fault.borrow_mut() = Some((label.to_owned(), action)));
}

pub(super) fn checkpoint(label: &str) -> Result<()> {
    let action = FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault
            .as_ref()
            .is_some_and(|(selected, _)| selected == label)
        {
            fault.take().map(|(_, action)| action)
        } else {
            None
        }
    });
    match action {
        Some(Action::Error) => Err(AdapterError::new(
            500,
            "configuration_test_fault",
            "Injected filesystem boundary failure.",
        )),
        Some(Action::Exit) => std::process::exit(73),
        Some(Action::ExternalEdit(path, raw)) => {
            std::fs::write(path, raw).expect("fixture external edit failed");
            Ok(())
        }
        None => Ok(()),
    }
}

struct Fixture {
    root: tempfile::TempDir,
    paths: ClientPaths,
}

const ENDPOINT: &str = "http://127.0.0.1:5001";
const CODEX: &[u8] =
    b"# fixture comment\r\nmodel = 'original'\r\nmodel_reasoning_effort = 'ultra'\r\n";
const CLAUDE: &[u8] =
    b"{\"env\":{\"ANTHROPIC_API_KEY\":\"fixture-only-secret\"},\"model\":\"keep\"}\r\n";

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("native-config-fixture-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        std::fs::write(root.path().join("fixture-owned"), b"native-config-test").unwrap();
        let paths = paths(root.path());
        std::fs::create_dir_all(paths.codex_config.as_ref().unwrap().parent().unwrap()).unwrap();
        std::fs::create_dir_all(paths.claude_settings.as_ref().unwrap().parent().unwrap()).unwrap();
        std::fs::write(paths.codex_config.as_ref().unwrap(), CODEX).unwrap();
        std::fs::write(paths.claude_settings.as_ref().unwrap(), CLAUDE).unwrap();
        Self { root, paths }
    }

    fn configure(&self) -> Result<Vec<ConfigurationChange>> {
        configure(
            &self.paths,
            ENDPOINT,
            &[Client::Codex, Client::Claude],
            &ConfigureOptions {
                model: Some("selected-model".into()),
                reasoning_effort: Some("max".into()),
                ..Default::default()
            },
        )
    }
}

fn paths(root: &Path) -> ClientPaths {
    ClientPaths {
        codex_config: Some(root.join("Codex settings").join("config.toml")),
        claude_settings: Some(root.join("Claude settings").join("settings.json")),
        backup_dir: root.join("backups"),
    }
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for item in std::fs::read_dir(root).unwrap() {
        let item = item.unwrap();
        if item.file_type().unwrap().is_dir() {
            result.extend(snapshot(&item.path()));
        } else {
            result.insert(item.path(), std::fs::read(item.path()).unwrap());
        }
    }
    result
}

fn child_root() -> Option<PathBuf> {
    let root = std::env::var_os("NATIVE_CONFIG_TEST_ROOT").map(PathBuf::from)?;
    let root = std::fs::canonicalize(root).unwrap();
    let parent = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    assert!(root.starts_with(parent));
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("native-config-fixture-")
    );
    assert!(std::fs::read(root.join("fixture-owned")).unwrap() == b"native-config-test");
    Some(root)
}

#[test]
fn native_crash_child() {
    let Some(root) = child_root() else {
        return;
    };
    let label = std::env::var("NATIVE_CONFIG_TEST_BOUNDARY").unwrap();
    inject(&label, Action::Exit);
    let paths = paths(&root);
    if std::env::var("NATIVE_CONFIG_TEST_OPERATION").unwrap() == "restore" {
        restore(&paths, &[Client::Codex, Client::Claude], false).unwrap();
    } else {
        configure(
            &paths,
            ENDPOINT,
            &[Client::Codex, Client::Claude],
            &ConfigureOptions {
                model: Some("selected-model".into()),
                reasoning_effort: Some("max".into()),
                ..Default::default()
            },
        )
        .unwrap();
    }
    panic!("Requested fixture crash boundary was not reached.");
}

fn crash(fixture: &Fixture, operation: &str, boundary: &str) {
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "config::tests::native_crash_child",
            "--nocapture",
        ])
        .env("NATIVE_CONFIG_TEST_ROOT", fixture.root.path())
        .env("NATIVE_CONFIG_TEST_OPERATION", operation)
        .env("NATIVE_CONFIG_TEST_BOUNDARY", boundary)
        .output()
        .unwrap();
    assert!(
        result.status.code() == Some(73),
        "The fixture did not exit at the requested boundary."
    );
}

#[test]
fn process_crash_recovery_covers_preparation_originals_targets_manifest_and_retirement() {
    for boundary in [
        "copy:0:after",
        "before-journal",
        "journal",
        "staged:0",
        "replaced:0",
        "edit:0",
        "replaced:2",
        "edit:3",
        "replaced:4",
        "before-retire-journal",
        "retired-journal",
    ] {
        let fixture = Fixture::new();
        crash(&fixture, "configure", boundary);
        let before_preview = snapshot(fixture.root.path());
        recover(&fixture.paths, true).unwrap();
        assert!(
            snapshot(fixture.root.path()) == before_preview,
            "Recovery preview wrote files."
        );
        recover(&fixture.paths, false).unwrap();
        if matches!(boundary, "copy:0:after" | "before-journal") {
            assert!(std::fs::read(fixture.paths.codex_config.as_ref().unwrap()).unwrap() == CODEX);
            assert!(
                std::fs::read(fixture.paths.claude_settings.as_ref().unwrap()).unwrap() == CLAUDE
            );
            fixture.configure().unwrap();
        }
        let statuses = inspect(
            &fixture.paths,
            ENDPOINT,
            &[Client::Codex, Client::Claude],
            Some("selected-model"),
            Some("max"),
        )
        .unwrap();
        assert!(
            statuses
                .iter()
                .all(|status| status.configured && status.restorable)
        );
        restore(&fixture.paths, &[Client::Codex, Client::Claude], false).unwrap();
        assert!(std::fs::read(fixture.paths.codex_config.as_ref().unwrap()).unwrap() == CODEX);
        assert!(std::fs::read(fixture.paths.claude_settings.as_ref().unwrap()).unwrap() == CLAUDE);
        assert!(!fixture.paths.backup_dir.join(JOURNAL).exists());
    }
}

#[test]
fn process_crash_during_restore_preserves_exact_originals_and_removes_only_managed_files() {
    for boundary in [
        "journal",
        "replaced:0",
        "edit:1",
        "replaced:2",
        "replaced:3",
        "before-retire-journal",
        "retired-journal",
    ] {
        let fixture = Fixture::new();
        fixture.configure().unwrap();
        crash(&fixture, "restore", boundary);
        recover(&fixture.paths, false).unwrap();
        assert!(std::fs::read(fixture.paths.codex_config.as_ref().unwrap()).unwrap() == CODEX);
        assert!(std::fs::read(fixture.paths.claude_settings.as_ref().unwrap()).unwrap() == CLAUDE);
        assert!(!fixture.paths.backup_dir.join(MANIFEST).exists());
        assert!(!fixture.paths.backup_dir.join("codex.original").exists());
        assert!(!fixture.paths.backup_dir.join("claude.original").exists());
    }
}

#[test]
fn failure_before_manifest_is_recovered_by_next_mutating_invocation() {
    let fixture = Fixture::new();
    inject("before-edit:4", Action::Error);
    assert!(fixture.configure().is_err());
    assert!(fixture.paths.backup_dir.join(JOURNAL).exists());
    assert!(inspect(&fixture.paths, ENDPOINT, &[Client::Codex], None, None).is_err());
    let changes = fixture.configure().unwrap();
    assert!(
        changes
            .iter()
            .any(|change| change.action == "recover configure")
    );
    assert!(!fixture.paths.backup_dir.join(JOURNAL).exists());
    assert!(std::fs::read(fixture.paths.backup_dir.join("codex.original")).unwrap() == CODEX);
}

#[test]
fn external_edit_during_transaction_blocks_all_further_changes_and_recovery() {
    let fixture = Fixture::new();
    let edited = b"# external fixture-private-value\nmodel = 'user-choice'\n".to_vec();
    inject(
        "before-edit:2",
        Action::ExternalEdit(fixture.paths.codex_config.clone().unwrap(), edited.clone()),
    );
    let error = fixture.configure().unwrap_err();
    assert_eq!(error.status, 409);
    assert!(!error.message.contains("fixture-private-value"));
    let saved = snapshot(fixture.root.path());
    assert!(recover(&fixture.paths, false).is_err());
    assert!(snapshot(fixture.root.path()) == saved);
    assert!(std::fs::read(fixture.paths.codex_config.as_ref().unwrap()).unwrap() == edited);
    assert!(std::fs::read(fixture.paths.claude_settings.as_ref().unwrap()).unwrap() == CLAUDE);
    assert!(std::fs::read(fixture.paths.backup_dir.join("codex.original")).unwrap() == CODEX);
}

#[test]
fn changed_recovery_images_and_completed_target_edits_are_never_overwritten() {
    for tamper_image in [true, false] {
        let fixture = Fixture::new();
        inject("edit:2", Action::Error);
        fixture.configure().unwrap_err();
        let journal =
            documents::json(&std::fs::read(fixture.paths.backup_dir.join(JOURNAL)).unwrap())
                .unwrap();
        let target = if tamper_image {
            fixture
                .paths
                .backup_dir
                .join(format!(
                    ".native-configuration-{}",
                    journal["id"].as_str().unwrap()
                ))
                .join("02.after")
        } else {
            fixture.paths.codex_config.clone().unwrap()
        };
        std::fs::write(
            target,
            if tamper_image {
                b"# unknown fixture-private-value\n".as_slice()
            } else {
                CODEX
            },
        )
        .unwrap();
        let saved = snapshot(fixture.root.path());
        let error = recover(&fixture.paths, false).unwrap_err();
        assert_eq!(error.status, 409);
        assert!(!error.message.contains("fixture-private-value"));
        assert!(snapshot(fixture.root.path()) == saved);
    }
}

#[test]
fn recovery_refuses_path_redirection_and_unknown_versions() {
    for redirect in [true, false] {
        let fixture = Fixture::new();
        inject("journal", Action::Error);
        fixture.configure().unwrap_err();
        let path = fixture.paths.backup_dir.join(JOURNAL);
        let mut journal = documents::json(&std::fs::read(&path).unwrap()).unwrap();
        let victim = fixture.root.path().join("unrelated.toml");
        std::fs::write(&victim, b"keep = true\n").unwrap();
        if redirect {
            journal["targets"]["codex"] = filesystem::path_text(&victim).unwrap().into();
        } else {
            journal["version"] = 999.into();
        }
        std::fs::write(path, documents::serialize(&journal).unwrap()).unwrap();
        let saved = snapshot(fixture.root.path());
        assert!(recover(&fixture.paths, false).is_err());
        assert!(snapshot(fixture.root.path()) == saved);
    }
}

#[test]
fn different_backup_directories_share_the_same_kernel_target_lock() {
    let fixture = Fixture::new();
    let held_paths = fixture.paths.clone();
    let mut other = fixture.paths.clone();
    other.backup_dir = fixture.root.path().join("different-backups");
    let (ready_send, ready) = mpsc::channel();
    let (release, release_receive) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let resolved = filesystem::resolve(&held_paths, &[Client::Codex]).unwrap();
        let _locks = transaction::lock(&held_paths, &resolved).unwrap();
        ready_send.send(()).unwrap();
        let _ = release_receive.recv();
    });
    ready.recv_timeout(Duration::from_secs(10)).unwrap();
    let result = configure(
        &other,
        ENDPOINT,
        &[Client::Codex],
        &ConfigureOptions::default(),
    );
    release.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(result.unwrap_err().status, 409);
    assert!(!other.backup_dir.exists());
    configure(
        &other,
        ENDPOINT,
        &[Client::Codex],
        &ConfigureOptions::default(),
    )
    .unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn mac_atomic_swap_crash_recovers_both_configure_and_restore() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for operation in ["configure", "restore"] {
        let fixture = Fixture::new();
        let target = fixture.paths.codex_config.as_ref().unwrap();
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let initial = std::fs::metadata(target).unwrap();
        if operation == "restore" {
            fixture.configure().unwrap();
        }
        crash(&fixture, operation, "mac-replace:swapped");
        let preview = snapshot(fixture.root.path());
        recover(&fixture.paths, true).unwrap();
        assert!(snapshot(fixture.root.path()) == preview);
        recover(&fixture.paths, false).unwrap();
        if operation == "configure" {
            assert!(
                inspect(&fixture.paths, ENDPOINT, &[Client::Codex], None, None).unwrap()[0]
                    .configured
            );
            restore(&fixture.paths, &[Client::Codex, Client::Claude], false).unwrap();
        }
        assert_eq!(std::fs::read(target).unwrap(), CODEX);
        let restored = std::fs::metadata(target).unwrap();
        assert_eq!(restored.mode() & 0o777, 0o640);
        assert_eq!(
            (restored.uid(), restored.gid()),
            (initial.uid(), initial.gid())
        );
        assert!(!fixture.paths.backup_dir.join(JOURNAL).exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn mac_swap_captures_a_racing_editor_without_deleting_its_bytes() {
    let fixture = Fixture::new();
    let edited = b"model = 'concurrent-editor'\n".to_vec();
    inject(
        "mac-replace:before-swap",
        Action::ExternalEdit(fixture.paths.codex_config.clone().unwrap(), edited.clone()),
    );
    assert_eq!(fixture.configure().unwrap_err().status, 409);
    let saved = snapshot(fixture.root.path());
    assert!(saved.iter().any(
        |(path, bytes)| path.extension().is_some_and(|ext| ext == "displaced") && bytes == &edited
    ));
    assert_eq!(recover(&fixture.paths, false).unwrap_err().status, 409);
    assert!(snapshot(fixture.root.path()) == saved);
}

#[cfg(target_os = "macos")]
#[test]
fn mac_recovery_images_are_private_without_broadening_existing_configuration_modes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let fixture = Fixture::new();
    let target = fixture.paths.codex_config.as_ref().unwrap();
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o640)).unwrap();
    inject("journal", Action::Error);
    fixture.configure().unwrap_err();
    let user = unsafe { libc::geteuid() };
    for path in snapshot(&fixture.paths.backup_dir).keys() {
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), user);
    }
    assert_eq!(
        std::fs::metadata(&fixture.paths.backup_dir).unwrap().mode() & 0o777,
        0o700
    );
    recover(&fixture.paths, false).unwrap();
    assert_eq!(std::fs::metadata(target).unwrap().mode() & 0o777, 0o640);
}

#[cfg(target_os = "macos")]
#[test]
fn mac_system_aliases_work_without_accepting_user_symlinks() {
    for directory in ["/tmp", "/var/tmp"] {
        let root = tempfile::Builder::new()
            .prefix("github-adapter-alias-")
            .tempdir_in(directory)
            .unwrap();
        let selected = ClientPaths {
            codex_config: Some(root.path().join("Codex space 雪").join("config.toml")),
            claude_settings: None,
            backup_dir: root.path().join("backup space"),
        };
        configure(
            &selected,
            ENDPOINT,
            &[Client::Codex],
            &ConfigureOptions::default(),
        )
        .unwrap();
        let canonical = std::fs::canonicalize(selected.codex_config.as_ref().unwrap()).unwrap();
        assert_eq!(
            filesystem::key(selected.codex_config.as_ref().unwrap()).unwrap(),
            filesystem::key(&canonical).unwrap()
        );
        restore(&selected, &[Client::Codex], false).unwrap();
        assert!(!selected.codex_config.as_ref().unwrap().exists());
        let redirected = root.path().join("redirected");
        std::os::unix::fs::symlink(
            selected.codex_config.as_ref().unwrap().parent().unwrap(),
            &redirected,
        )
        .unwrap();
        let redirected_paths = ClientPaths {
            codex_config: Some(redirected.join("config.toml")),
            ..selected
        };
        assert!(read_codex_settings(&redirected_paths).is_err());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn mac_case_and_unicode_aliases_share_locks_on_insensitive_volumes() {
    use std::os::fd::AsRawFd;
    let fixture = Fixture::new();
    let parent = std::fs::File::open(fixture.root.path()).unwrap();
    let sensitive = unsafe { libc::fpathconf(parent.as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
    assert!(sensitive == 0 || sensitive == 1);
    let first = filesystem::key(&fixture.root.path().join("MiXeD-é.toml")).unwrap();
    let second = filesystem::key(&fixture.root.path().join("mixed-e\u{301}.toml")).unwrap();
    let _held = filesystem::MutexGuard::acquire(&format!("target:{first}")).unwrap();
    let other = filesystem::MutexGuard::acquire(&format!("target:{second}"));
    if sensitive == 0 {
        assert_eq!(first, second);
        assert_eq!(other.err().unwrap().status, 409);
    } else {
        assert_ne!(first, second);
        assert!(other.is_ok());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn mac_unicode_normalization_aliases_share_locks_on_both_volume_types() {
    let fixture = Fixture::new();
    let composed = filesystem::key(&fixture.root.path().join("same-case-é.toml")).unwrap();
    let decomposed = filesystem::key(&fixture.root.path().join("same-case-e\u{301}.toml")).unwrap();
    assert_eq!(composed, decomposed);
    let _held = filesystem::MutexGuard::acquire(&format!("target:{composed}")).unwrap();
    assert_eq!(
        filesystem::MutexGuard::acquire(&format!("target:{decomposed}"))
            .err()
            .unwrap()
            .status,
        409
    );
}

#[cfg(target_os = "macos")]
#[test]
fn mac_login_registration_is_private_and_preserves_a_racing_editor() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let fixture = Fixture::new();
    let path = fixture
        .root
        .path()
        .join("Library/LaunchAgents/fixture.plist");
    let expected = b"<plist>fixture-owned</plist>";
    on_demand::update_login_registration(&path, expected, true).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    let edited = b"<plist>concurrent-editor</plist>".to_vec();
    inject(
        "mac-login:before-remove",
        Action::ExternalEdit(path.clone(), edited.clone()),
    );
    assert_eq!(
        on_demand::update_login_registration(&path, expected, false)
            .unwrap_err()
            .status,
        409
    );
    assert_eq!(std::fs::read(&path).unwrap(), edited);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(on_demand::update_login_registration(&path, &edited, false).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), edited);
}

#[cfg(target_os = "macos")]
#[test]
fn mac_login_registration_rejects_redirected_parents_without_creating_outside() {
    let fixture = Fixture::new();
    let outside = fixture.root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.root.path().join("Library")).unwrap();
    let path = fixture
        .root
        .path()
        .join("Library/LaunchAgents/fixture.plist");
    assert!(on_demand::update_login_registration(&path, b"fixture", true).is_err());
    assert!(!outside.join("LaunchAgents").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn mac_login_registration_rejects_a_fifo_without_blocking_or_removing_it() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    let fixture = Fixture::new();
    let path = fixture
        .root
        .path()
        .join("Library/LaunchAgents/fixture.plist");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    for arm in [true, false] {
        let selected = path.clone();
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(on_demand::update_login_registration(
                &selected, b"fixture", arm,
            ));
        });
        assert!(
            receive
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }
}

#[test]
fn malformed_json_reserved_keys_and_endpoint_validation_are_pure() {
    for raw in [
        br#"{"a":1,"a":2}"#.as_slice(),
        br#"{"env":{"x":1,"x":2}}"#,
        br#"{"list":[{"x":1,"x":2}]}"#,
        br#"{"x":NaN}"#,
        br#"{"x":1e999}"#,
        b"[]",
        b"\xff",
    ] {
        assert!(documents::json(raw).is_err());
    }
    let value = documents::json(
        br#"{"$serde_json::private::Number":"untouched","large":123456789012345678901234567890}"#,
    )
    .unwrap();
    assert_eq!(value["$serde_json::private::Number"], "untouched");
    for endpoint in [
        "https://127.0.0.1:5001",
        "http://example.invalid",
        "http://0.0.0.0:5001",
        "http://127.0.0.1:0",
        "http://localhost:65536",
        "http://localhost:",
        "http://127.0.0.1/v1",
        "http://fixture-private-value@localhost:5001",
        "http://localhost:5001?token=fixture-private-value",
        "http://localhost\n",
    ] {
        let error = documents::endpoint(endpoint).unwrap_err();
        assert!(!error.message.contains("fixture-private-value"));
    }
    assert_eq!(
        documents::endpoint(" http://[::1]:5001/ ").unwrap(),
        "http://[::1]:5001"
    );
    assert_eq!(
        documents::endpoint("http://localhost").unwrap(),
        "http://localhost"
    );
}

#[test]
fn restore_unmanages_a_file_even_when_managed_updates_returned_to_original_bytes() {
    let fixture = Fixture::new();
    let original = format!("openai_base_url = \"{ENDPOINT}\"\nmodel = \"original\"\n").into_bytes();
    std::fs::write(fixture.paths.codex_config.as_ref().unwrap(), &original).unwrap();
    for model in ["changed", "original"] {
        configure(
            &fixture.paths,
            ENDPOINT,
            &[Client::Codex],
            &ConfigureOptions {
                model: Some(model.into()),
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert!(std::fs::read(fixture.paths.codex_config.as_ref().unwrap()).unwrap() == original);
    restore(&fixture.paths, &[Client::Codex], false).unwrap();
    assert!(std::fs::read(fixture.paths.codex_config.as_ref().unwrap()).unwrap() == original);
    assert!(!fixture.paths.backup_dir.join(MANIFEST).exists());
    assert!(!fixture.paths.backup_dir.join("codex.original").exists());
}

#[test]
fn interrupted_native_displacement_and_partial_owned_staging_are_recoverable() {
    for partial_stage in [true, false] {
        let fixture = Fixture::new();
        inject("staged:2", Action::Error);
        fixture.configure().unwrap_err();
        let journal =
            documents::json(&std::fs::read(fixture.paths.backup_dir.join(JOURNAL)).unwrap())
                .unwrap();
        let id = journal["id"].as_str().unwrap();
        let target = fixture.paths.codex_config.as_ref().unwrap();
        let parent = target.parent().unwrap();
        if partial_stage {
            let staged = parent.join(format!(".github-adapter-{id}-02.tmp"));
            let bytes = std::fs::read(&staged).unwrap();
            std::fs::write(staged, &bytes[..bytes.len() / 2]).unwrap();
        } else {
            let displaced = parent.join(format!(".github-adapter-{id}-02.displaced"));
            std::fs::rename(target, displaced).unwrap();
        }
        recover(&fixture.paths, false).unwrap();
        assert!(
            inspect(
                &fixture.paths,
                ENDPOINT,
                &[Client::Codex, Client::Claude],
                Some("selected-model"),
                Some("max"),
            )
            .unwrap()
            .iter()
            .all(|status| status.configured && status.restorable)
        );
    }
}

#[test]
fn original_and_v1_manifest_corruption_never_rebaseline_or_redirect_restore() {
    for mode in ["original", "target", "extra-field", "missing-field"] {
        let fixture = Fixture::new();
        fixture.configure().unwrap();
        let manifest_path = fixture.paths.backup_dir.join(MANIFEST);
        let mut manifest = documents::json(&std::fs::read(&manifest_path).unwrap()).unwrap();
        match mode {
            "original" => {
                std::fs::write(
                    fixture.paths.backup_dir.join("codex.original"),
                    b"# unknown original\n",
                )
                .unwrap();
            }
            "target" => {
                let victim = fixture.root.path().join("unrelated.toml");
                std::fs::write(&victim, b"keep = true\n").unwrap();
                manifest["entries"]["codex"]["target"] =
                    filesystem::path_text(&victim).unwrap().into();
                std::fs::write(&manifest_path, documents::serialize(&manifest).unwrap()).unwrap();
            }
            "extra-field" => {
                manifest["entries"]["codex"]["unrecognized"] = true.into();
                std::fs::write(&manifest_path, documents::serialize(&manifest).unwrap()).unwrap();
            }
            _ => {
                manifest["entries"]["codex"]
                    .as_object_mut()
                    .unwrap()
                    .remove("original_backup");
                std::fs::write(&manifest_path, documents::serialize(&manifest).unwrap()).unwrap();
            }
        }
        let before = snapshot(fixture.root.path());
        assert!(fixture.configure().is_err());
        assert!(restore(&fixture.paths, &[Client::Codex, Client::Claude], false).is_err());
        assert!(snapshot(fixture.root.path()) == before);
    }
}

#[test]
fn legacy_lock_files_are_preserved_and_block_native_recovery_too() {
    let fixture = Fixture::new();
    inject("journal", Action::Error);
    fixture.configure().unwrap_err();
    let lock = fixture.paths.backup_dir.join(".configuration.lock");
    std::fs::write(&lock, b"legacy fixture lock").unwrap();
    let before = snapshot(fixture.root.path());
    assert_eq!(recover(&fixture.paths, false).unwrap_err().status, 409);
    assert!(snapshot(fixture.root.path()) == before);
}

#[test]
fn native_default_paths_child() {
    let Ok(mode) = std::env::var("NATIVE_CONFIG_TEST_DEFAULTS") else {
        return;
    };
    let root = child_root().unwrap();
    let client = if mode == "codex" {
        Client::Codex
    } else {
        Client::Claude
    };
    let result = default_paths(None, None, None, &[client]).unwrap();
    let expected = root.join("selected-home").join(if client == Client::Codex {
        "config.toml"
    } else {
        "settings.json"
    });
    assert_eq!(
        filesystem::key(result.path(client).unwrap()).unwrap(),
        filesystem::key(&expected).unwrap()
    );
    assert!(
        result
            .path(if client == Client::Codex {
                Client::Claude
            } else {
                Client::Codex
            })
            .is_none()
    );
    assert_eq!(
        filesystem::key(&result.backup_dir).unwrap(),
        filesystem::key(
            &root
                .join("selected-state")
                .join("GitHubAdapter")
                .join("client-backups")
        )
        .unwrap()
    );
    assert!(!root.join("selected-home").exists());
    assert!(!root.join("selected-state").exists());
    assert!(default_paths(None, None, None, &[Client::Codex, Client::Claude]).is_err());
}

#[test]
fn selected_environment_defaults_ignore_invalid_unselected_client_and_home() {
    for client in ["codex", "claude"] {
        let fixture = Fixture::new();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "config::tests::native_default_paths_child",
                "--nocapture",
            ])
            .env("NATIVE_CONFIG_TEST_ROOT", fixture.root.path())
            .env("NATIVE_CONFIG_TEST_DEFAULTS", client)
            .env("USERPROFILE", "")
            .env("LOCALAPPDATA", fixture.root.path().join("selected-state"))
            .env("HOME", "")
            .env("XDG_STATE_HOME", fixture.root.path().join("selected-state"));
        if client == "codex" {
            command
                .env("CODEX_HOME", fixture.root.path().join("selected-home"))
                .env("CLAUDE_CONFIG_DIR", " ");
        } else {
            command
                .env(
                    "CLAUDE_CONFIG_DIR",
                    fixture.root.path().join("selected-home"),
                )
                .env("CODEX_HOME", " ");
        }
        assert!(command.output().unwrap().status.success());
    }
}

#[test]
#[cfg(windows)]
fn recovery_images_and_journal_have_protected_single_user_dacls() {
    assert_recovery_dacls(&Fixture::new());
}

#[test]
#[cfg(windows)]
fn recovery_images_and_journal_dacl_inspection_supports_long_paths() {
    use std::os::windows::ffi::OsStrExt;

    let mut fixture = Fixture::new();
    fixture.paths.backup_dir = fixture
        .paths
        .backup_dir
        .join("long-".repeat(24))
        .join("deep-".repeat(24));
    assert!(fixture.paths.backup_dir.as_os_str().encode_wide().count() > 260);
    assert_recovery_dacls(&fixture);
}

#[cfg(windows)]
fn assert_recovery_dacls(fixture: &Fixture) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetFileSecurityW, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, SE_DACL_PROTECTED,
    };
    inject("journal", Action::Error);
    fixture.configure().unwrap_err();
    let journal =
        documents::json(&std::fs::read(fixture.paths.backup_dir.join(JOURNAL)).unwrap()).unwrap();
    let directory = fixture.paths.backup_dir.join(format!(
        ".native-configuration-{}",
        journal["id"].as_str().unwrap()
    ));
    for path in [
        fixture.paths.backup_dir.join(JOURNAL),
        directory.clone(),
        directory.join("00.after"),
        directory.join("02.before"),
    ] {
        // Use a verbatim Windows path so the raw ACL probe works in deep checkouts.
        let path = std::fs::canonicalize(path).unwrap();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut size = 0;
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut size,
            );
        }
        assert!(size > 0);
        let mut storage = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        assert_ne!(
            unsafe {
                GetFileSecurityW(
                    wide.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    storage.as_mut_ptr().cast(),
                    size,
                    &mut size,
                )
            },
            0
        );
        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe {
                GetSecurityDescriptorControl(
                    storage.as_mut_ptr().cast(),
                    &mut control,
                    &mut revision,
                )
            },
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0);
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(
                    storage.as_mut_ptr().cast(),
                    &mut present,
                    &mut acl,
                    &mut defaulted,
                )
            },
            0
        );
        assert_ne!(present, 0);
        assert!(!acl.is_null());
        assert_eq!(unsafe { (*acl).AceCount }, 1);
    }
}

struct ImageFixture(Fixture);
impl ImageFixture {
    fn new() -> Self {
        Self(Fixture::new())
    }
    fn program(&self) -> PathBuf {
        std::env::current_exe().unwrap()
    }
    fn provider(&self) -> adapter_runtime::images::ImageConfig {
        adapter_runtime::images::ImageConfig {
            endpoint: "https://images.example.invalid/v1/images/generations".into(),
            model: "synthetic-image-model".into(),
            protocol: adapter_runtime::images::ImageProtocol::OpenaiImages,
        }
    }
    fn enable(&self) -> serde_json::Value {
        images::enable(
            &self.0.paths,
            &self.program(),
            self.provider(),
            "synthetic-image-key",
        )
        .unwrap()
    }
}
impl Drop for ImageFixture {
    fn drop(&mut self) {
        let _ = images::disable(&self.0.paths);
    }
}

#[test]
fn image_registration_is_private_reversible_and_preserves_unrelated_edits() {
    let fixture = ImageFixture::new();
    let original = std::fs::read(fixture.0.paths.codex_config.as_ref().unwrap()).unwrap();
    let enabled = fixture.enable();
    assert_eq!(enabled["mcp_registered"], true);
    assert_eq!(enabled["skill_managed"], true);
    let (provider, key) = images::snapshot(&fixture.0.paths).unwrap();
    assert_eq!(key, "synthetic-image-key");
    assert_eq!(provider, fixture.provider());
    let receipt =
        std::fs::read_to_string(fixture.0.paths.backup_dir.join("image-provider.json")).unwrap();
    assert!(!receipt.contains("synthetic-image-key"));
    let path = fixture.0.paths.codex_config.as_ref().unwrap();
    let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(path).unwrap().parse().unwrap();
    doc["model"] = toml_edit::value("user-edit");
    doc["theme"] = toml_edit::value("dark");
    std::fs::write(path, doc.to_string()).unwrap();
    let disabled = images::disable(&fixture.0.paths).unwrap();
    assert_eq!(disabled["enabled"], false);
    let doc: toml_edit::DocumentMut = std::fs::read_to_string(path).unwrap().parse().unwrap();
    assert_eq!(doc["model"].as_str(), Some("user-edit"));
    assert_eq!(doc["theme"].as_str(), Some("dark"));
    assert!(doc.get("mcp_servers").is_none());
    assert!(images::snapshot(&fixture.0.paths).is_err());
    assert!(!original.is_empty());
}

#[test]
fn image_configuration_does_not_take_over_another_mcp_or_user_edited_skill() {
    let fixture = ImageFixture::new();
    let path = fixture.0.paths.codex_config.as_ref().unwrap();
    std::fs::write(
        path,
        b"[mcp_servers.github_adapter_image]\ncommand='other-tool'\n",
    )
    .unwrap();
    let before = std::fs::read(path).unwrap();
    assert!(
        images::enable(
            &fixture.0.paths,
            &fixture.program(),
            fixture.provider(),
            "synthetic-image-key"
        )
        .is_err()
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
    std::fs::write(path, CODEX).unwrap();
    fixture.enable();
    let skill = path
        .parent()
        .unwrap()
        .join("skills/github-adapter-image/SKILL.md");
    let edited = b"# user-owned image guidance\n";
    std::fs::write(&skill, edited).unwrap();
    images::repair(&fixture.0.paths, &fixture.program()).unwrap();
    assert_eq!(std::fs::read(&skill).unwrap(), edited);
    images::disable(&fixture.0.paths).unwrap();
    assert_eq!(std::fs::read(&skill).unwrap(), edited);
}

#[test]
fn image_skill_publication_preserves_an_edit_after_ownership_validation() {
    let fixture = ImageFixture::new();
    fixture.enable();
    let skill = fixture
        .0
        .paths
        .codex_config
        .as_ref()
        .unwrap()
        .parent()
        .unwrap()
        .join("skills/github-adapter-image/SKILL.md");
    let edited = b"# independently edited after authorization\n";
    inject(
        "image:skill-authorized",
        Action::ExternalEdit(skill.clone(), edited.to_vec()),
    );
    let result = images::repair(&fixture.0.paths, &fixture.program());
    assert!(
        result.is_err(),
        "A newly edited skill must not be adopted as the authorized preimage."
    );
    assert_eq!(std::fs::read(&skill).unwrap(), edited);
    assert_eq!(images::status(&fixture.0.paths).unwrap()["pending"], true);
    assert!(images::snapshot(&fixture.0.paths).is_err());
    images::disable(&fixture.0.paths).unwrap();
    assert_eq!(std::fs::read(&skill).unwrap(), edited);
    assert!(!snapshot(fixture.0.root.path()).keys().any(|path| {
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".image-")
    }));
}

#[test]
fn interrupted_image_setup_and_disable_recover_without_generating() {
    for point in ["image:state", "image:config", "image:skill"] {
        let fixture = ImageFixture::new();
        inject(point, Action::Error);
        assert!(
            images::enable(
                &fixture.0.paths,
                &fixture.program(),
                fixture.provider(),
                "synthetic-image-key"
            )
            .is_err()
        );
        assert_eq!(images::status(&fixture.0.paths).unwrap()["pending"], true);
        assert!(images::snapshot(&fixture.0.paths).is_err());
        images::startup(&fixture.0.paths, &fixture.program()).unwrap();
        let ready = images::status(&fixture.0.paths).unwrap();
        assert_eq!(ready["pending"], false, "{point}");
        assert_eq!(ready["skill_managed"], true, "{point}");
        assert_eq!(
            images::snapshot(&fixture.0.paths).unwrap().1,
            "synthetic-image-key"
        );
        inject("image:disable", Action::Error);
        assert!(images::disable(&fixture.0.paths).is_err());
        assert!(images::snapshot(&fixture.0.paths).is_err());
        images::startup(&fixture.0.paths, &fixture.program()).unwrap();
        assert_eq!(images::status(&fixture.0.paths).unwrap()["enabled"], false);
    }
}

#[test]
fn image_provider_updates_bind_each_key_to_its_config_and_resume_pending_changes() {
    let fixture = ImageFixture::new();
    fixture.enable();
    let mut other = fixture.provider();
    other.endpoint = "https://another.example.invalid/images/generations".into();
    inject("image:config", Action::Error);
    assert!(
        images::enable(
            &fixture.0.paths,
            &fixture.program(),
            other.clone(),
            "second-image-key"
        )
        .is_err()
    );
    assert!(images::snapshot(&fixture.0.paths).is_err());
    images::startup(&fixture.0.paths, &fixture.program()).unwrap();
    let (config, key) = images::snapshot(&fixture.0.paths).unwrap();
    assert_eq!(config, other);
    assert_eq!(key, "second-image-key");
    let file = fixture.0.paths.backup_dir.join("image-provider.json");
    let before = std::fs::read(&file).unwrap();
    let mut state: serde_json::Value = serde_json::from_slice(&before).unwrap();
    state["provider"]["endpoint"] =
        serde_json::json!("https://tampered.example.invalid/images/generations");
    std::fs::write(&file, serde_json::to_vec(&state).unwrap()).unwrap();
    let rejected = images::snapshot(&fixture.0.paths).is_err();
    std::fs::write(&file, before).unwrap();
    assert!(rejected);
}

#[test]
fn image_helper_maintenance_is_resumable_and_validates_before_mutating() {
    for explicit in [false, true] {
        for point in ["image:config", "image:skill"] {
            let fixture = ImageFixture::new();
            fixture.enable();
            let maintain = |program: &Path| {
                if explicit {
                    images::repair(&fixture.0.paths, program).map(|_| ())
                } else {
                    images::startup(&fixture.0.paths, program)
                }
            };
            let helper = fixture.0.root.path().join("new-image-helper.exe");
            let before = snapshot(fixture.0.root.path());
            assert!(maintain(&helper).is_err());
            assert_eq!(snapshot(fixture.0.root.path()), before);

            // A path fixture only: maintenance must not execute the helper.
            std::fs::write(&helper, b"synthetic path only").unwrap();
            inject(point, Action::Error);
            assert!(maintain(&helper).is_err());
            let pending = images::status(&fixture.0.paths).unwrap();
            assert_eq!(pending["pending"], true, "{explicit}/{point}");
            assert_eq!(pending["helper"], serde_json::json!(helper));
            assert_eq!(pending["mcp_registered"], true);
            assert!(images::snapshot(&fixture.0.paths).is_err());

            maintain(&helper).unwrap();
            let ready = images::status(&fixture.0.paths).unwrap();
            assert_eq!(ready["pending"], false);
            assert_eq!(ready["helper"], serde_json::json!(helper));
            assert_eq!(ready["mcp_registered"], true);
            assert_eq!(ready["skill_managed"], true);
            assert_eq!(
                images::snapshot(&fixture.0.paths).unwrap(),
                (fixture.provider(), "synthetic-image-key".to_owned())
            );
            let finished = snapshot(fixture.0.root.path());
            maintain(&helper).unwrap();
            assert_eq!(snapshot(fixture.0.root.path()), finished);
        }
    }
}

#[test]
fn image_startup_does_not_restore_a_user_deleted_mcp_entry() {
    let fixture = ImageFixture::new();
    fixture.enable();
    let path = fixture.0.paths.codex_config.as_ref().unwrap();
    std::fs::write(path, CODEX).unwrap();
    let another = fixture.0.root.path().join("new-image-helper.exe");
    std::fs::write(&another, b"synthetic path only").unwrap();
    assert!(images::startup(&fixture.0.paths, &another).is_err());
    assert_eq!(std::fs::read(path).unwrap(), CODEX);
    images::repair(&fixture.0.paths, &another).unwrap();
    assert_eq!(
        images::status(&fixture.0.paths).unwrap()["mcp_registered"],
        true
    );
}

#[test]
fn legacy_image_provider_can_be_disabled_when_new_endpoint_policy_rejects_it() {
    let fixture = ImageFixture::new();
    fixture.enable();
    let path = fixture.0.paths.backup_dir.join("image-provider.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let old_key = state["key_id"].as_str().unwrap().to_owned();
    let service = format!(
        "GitHubAdapter.ImageProvider.{}",
        filesystem::hash(state["codex"].as_str().unwrap().to_lowercase().as_bytes())
    );
    let secret = images::credentials::read(&service, &old_key)
        .unwrap()
        .unwrap_or_else(|| panic!("Synthetic fixture namespace mismatch: scope={service}, codex={}, native_snapshot_ok={}, status={}", state["codex"], images::snapshot(&fixture.0.paths).is_ok(), images::status(&fixture.0.paths).unwrap()));
    // 0.1.3 accepted a trailing space and the URL parser silently trimmed it.
    state["provider"]["endpoint"] = serde_json::json!(format!("{} ", fixture.provider().endpoint));
    let provider: adapter_runtime::images::ImageConfig =
        serde_json::from_value(state["provider"].clone()).unwrap();
    let key = format!(
        "{}-{}",
        &filesystem::hash(&serde_json::to_vec(&provider).unwrap())[..16],
        &old_key[17..]
    );
    state["key_id"] = serde_json::json!(key);
    struct Keys(String, [String; 2]);
    impl Drop for Keys {
        fn drop(&mut self) {
            for name in &self.1 {
                let _ = images::credentials::delete(&self.0, name);
            }
        }
    }
    let keys = Keys(service.clone(), [old_key.clone(), key.clone()]);
    images::credentials::write(&service, &key, &secret).unwrap();
    images::credentials::delete(&service, &old_key).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    let before = snapshot(fixture.0.root.path());
    assert!(images::repair(&fixture.0.paths, &fixture.program()).is_err());
    assert!(images::startup(&fixture.0.paths, &fixture.program()).is_err());
    assert_eq!(snapshot(fixture.0.root.path()), before);
    let snapshot = images::snapshot(&fixture.0.paths);
    let status = images::status(&fixture.0.paths);
    let disabled = images::disable(&fixture.0.paths);
    let remaining_key = images::credentials::read(&service, &key);
    // Also cleans both synthetic slots if fixture preparation or assertions panic.
    drop(keys);
    assert!(
        snapshot.is_err(),
        "A rejected provider must never reach generation."
    );
    assert_eq!(status.unwrap()["provider_valid"], false);
    assert_eq!(disabled.unwrap()["enabled"], false);
    assert!(remaining_key.unwrap().is_none());
    assert!(!path.exists());
    let config: toml_edit::DocumentMut =
        std::fs::read_to_string(fixture.0.paths.codex_config.as_ref().unwrap())
            .unwrap()
            .parse()
            .unwrap();
    assert!(config.get("mcp_servers").is_none());
}

#[test]
fn pending_image_transition_finishes_before_switching_to_a_third_helper() {
    for point in ["image:state", "image:config", "image:skill"] {
        let fixture = ImageFixture::new();
        fixture.enable();
        let second = fixture.0.root.path().join("second-helper.exe");
        let third = fixture.0.root.path().join("third-helper.exe");
        std::fs::write(&second, b"path fixture only").unwrap();
        std::fs::write(&third, b"path fixture only").unwrap();
        inject(point, Action::Error);
        assert!(
            images::enable(
                &fixture.0.paths,
                &second,
                fixture.provider(),
                "second-image-key"
            )
            .is_err()
        );
        images::startup(&fixture.0.paths, &third).unwrap();
        let state = images::status(&fixture.0.paths).unwrap();
        assert_eq!(state["pending"], false, "{point}");
        assert_eq!(state["helper"], serde_json::json!(third));
        assert_eq!(state["mcp_registered"], true);
        assert_eq!(state["skill_managed"], true);
        assert_eq!(
            images::snapshot(&fixture.0.paths).unwrap().1,
            "second-image-key"
        );
    }
}

#[test]
fn disabling_a_first_enable_interrupted_after_skill_publication_removes_its_skill() {
    for legacy in [false, true] {
        let fixture = ImageFixture::new();
        inject("image:skill", Action::Error);
        assert!(
            images::enable(
                &fixture.0.paths,
                &fixture.program(),
                fixture.provider(),
                "synthetic-image-key"
            )
            .is_err()
        );
        let receipt = fixture.0.paths.backup_dir.join("image-provider.json");
        if legacy {
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
            value.as_object_mut().unwrap().remove("pending_skill_hash");
            std::fs::write(&receipt, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        let skill = fixture
            .0
            .paths
            .codex_config
            .as_ref()
            .unwrap()
            .parent()
            .unwrap()
            .join("skills/github-adapter-image/SKILL.md");
        assert!(skill.is_file());
        images::disable(&fixture.0.paths).unwrap();
        assert!(!skill.exists(), "legacy={legacy}");
        assert!(!receipt.exists());
    }
}

#[test]
fn invalid_editable_configuration_does_not_prevent_image_key_revocation() {
    let fixture = ImageFixture::new();
    fixture.enable();
    let receipt = fixture.0.paths.backup_dir.join("image-provider.json");
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    let service = format!(
        "GitHubAdapter.ImageProvider.{}",
        filesystem::hash(state["codex"].as_str().unwrap().to_lowercase().as_bytes())
    );
    let key = state["key_id"].as_str().unwrap();
    assert!(images::credentials::read(&service, key).unwrap().is_some());
    let path = fixture.0.paths.codex_config.as_ref().unwrap();
    let original = std::fs::read(path).unwrap();
    let broken = b"not valid TOML = [";
    std::fs::write(path, broken).unwrap();
    let disabled = images::disable(&fixture.0.paths);
    let revoked = images::credentials::read(&service, key);
    let retained = std::fs::read(path).unwrap();
    let pending: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    // Restore the fixture before assertions so the baseline failure also cleans its key.
    std::fs::write(path, original).unwrap();
    images::disable(&fixture.0.paths).unwrap();
    assert!(disabled.is_err());
    assert!(revoked.unwrap().is_none());
    assert_eq!(retained, broken);
    assert_eq!(pending["enabled"], false);
    assert_eq!(pending["pending"], true);
}

#[test]
fn image_output_reports_its_reserved_resolved_path() {
    let root = tempfile::Builder::new()
        .prefix("reserved-image-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let supplied = root.path().join("nested/../image.png");
    let reserved = images::reserve_output(&supplied).unwrap();
    let reported = reserved.write(b"synthetic image bytes").unwrap();
    assert_eq!(reported, root.path().join("image.png"));
    assert_eq!(std::fs::read(&reported).unwrap(), b"synthetic image bytes");
    assert!(images::reserve_output(&supplied).is_err());
}

#[test]
fn image_skill_replanning_keeps_the_last_published_pending_identity() {
    let fixture = ImageFixture::new();
    fixture.enable();
    let receipt = fixture.0.paths.backup_dir.join("image-provider.json");
    let skill = fixture
        .0
        .paths
        .codex_config
        .as_ref()
        .unwrap()
        .parent()
        .unwrap()
        .join("skills/github-adapter-image/SKILL.md");
    let previous_intent = b"# owned skill from an interrupted earlier template revision\n";
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    state["pending"] = serde_json::json!(true);
    state["pending_skill_hash"] = serde_json::json!(filesystem::hash(previous_intent));
    std::fs::write(&receipt, serde_json::to_vec(&state).unwrap()).unwrap();
    std::fs::write(&skill, previous_intent).unwrap();
    inject("image:skill-authorized", Action::Error);
    assert!(images::repair(&fixture.0.paths, &fixture.program()).is_err());
    assert_eq!(std::fs::read(&skill).unwrap(), previous_intent);
    images::disable(&fixture.0.paths).unwrap();
    assert!(!skill.exists());
}

#[test]
fn quoted_home_image_output_reports_the_path_that_was_created() {
    let home = filesystem::absolute(Path::new("~"), true).unwrap();
    let root = tempfile::Builder::new()
        .prefix("adapter-image-path-fixture-")
        .tempdir_in(&home)
        .unwrap();
    let supplied = PathBuf::from("~")
        .join(root.path().file_name().unwrap())
        .join("image.png");
    let output = images::reserve_output(&supplied).unwrap();
    let path = output.write(b"synthetic image bytes").unwrap();
    assert_eq!(path, root.path().join("image.png"));
    assert_eq!(std::fs::read(path).unwrap(), b"synthetic image bytes");
    assert!(images::reserve_output(&supplied).is_err());
}

#[cfg(windows)]
#[test]
fn normal_and_verbatim_long_paths_share_configuration_io_and_lock_identity() {
    let fixture = Fixture::new();
    let long = fixture
        .root
        .path()
        .join("long-".repeat(24))
        .join("deep-".repeat(24));
    let selected = paths(&long);
    let target = selected.codex_config.as_ref().unwrap();
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, CODEX).unwrap();
    assert!(target.to_string_lossy().len() > 260);
    let verbatim = |path: &Path| PathBuf::from(format!(r"\\?\{}", path.display()));
    let alias = ClientPaths {
        codex_config: Some(verbatim(target)),
        claude_settings: None,
        backup_dir: verbatim(&selected.backup_dir),
    };
    assert_eq!(
        filesystem::key(target).unwrap(),
        filesystem::key(alias.codex_config.as_ref().unwrap()).unwrap()
    );
    for candidate in [&selected, &alias] {
        configure(
            candidate,
            ENDPOINT,
            &[Client::Codex],
            &ConfigureOptions::default(),
        )
        .unwrap();
        restore(candidate, &[Client::Codex], false).unwrap();
        assert_eq!(std::fs::read(target).unwrap(), CODEX);
    }
}

#[test]
fn pending_configuration_recovery_does_not_gate_image_key_revocation() {
    for startup in [false, true] {
        let fixture = ImageFixture::new();
        fixture.enable();
        let target = fixture.0.paths.codex_config.as_ref().unwrap();
        let before = std::fs::read(target).unwrap();
        let helper = fixture.0.root.path().join("journal-helper.exe");
        std::fs::write(&helper, b"path fixture only").unwrap();
        inject("journal", Action::Error);
        assert!(images::repair(&fixture.0.paths, &helper).is_err());
        let receipt = fixture.0.paths.backup_dir.join("image-provider.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
        let service = format!(
            "GitHubAdapter.ImageProvider.{}",
            filesystem::hash(state["codex"].as_str().unwrap().to_lowercase().as_bytes())
        );
        let key = state["key_id"].as_str().unwrap().to_owned();
        if startup {
            state["enabled"] = serde_json::json!(false);
            std::fs::write(&receipt, serde_json::to_vec(&state).unwrap()).unwrap();
        }
        std::fs::write(target, b"broken = [").unwrap();
        let result = if startup {
            images::startup(&fixture.0.paths, &helper)
        } else {
            images::disable(&fixture.0.paths).map(|_| ())
        };
        let revoked = images::credentials::read(&service, &key).unwrap().is_none();
        std::fs::write(target, before).unwrap();
        images::disable(&fixture.0.paths).unwrap();
        assert!(result.is_err());
        assert!(revoked, "startup={startup}");
    }
}

#[test]
fn image_credential_write_failure_restores_the_prior_registration() {
    use images::credentials::{self, Operation as CredentialOperation};
    for existing in [false, true] {
        let fixture = ImageFixture::new();
        if existing {
            fixture.enable();
        }
        let receipt = fixture.0.paths.backup_dir.join("image-provider.json");
        let before = filesystem::read(&receipt).unwrap();
        let config_before = std::fs::read(fixture.0.paths.codex_config.as_ref().unwrap()).unwrap();
        let previous = existing.then(|| images::snapshot(&fixture.0.paths).unwrap());
        credentials::take_calls();
        credentials::fail_next(CredentialOperation::Write);
        let mut provider = fixture.provider();
        provider.model = "different-provider-model".into();
        let error = images::enable(
            &fixture.0.paths,
            &fixture.program(),
            provider,
            "replacement-synthetic-key",
        )
        .unwrap_err();
        assert_eq!(error.code, "credential_test_failure");
        let calls = credentials::take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, CredentialOperation::Write);
        assert_eq!(calls[1].0, CredentialOperation::Delete);
        assert_eq!((&calls[0].1, &calls[0].2), (&calls[1].1, &calls[1].2));
        assert!(
            credentials::read(&calls[0].1, &calls[0].2)
                .unwrap()
                .is_none()
        );
        assert_eq!(filesystem::read(&receipt).unwrap(), before);
        assert_eq!(
            std::fs::read(fixture.0.paths.codex_config.as_ref().unwrap()).unwrap(),
            config_before
        );
        if let Some(previous) = previous {
            assert_eq!(images::snapshot(&fixture.0.paths).unwrap(), previous);
        }
    }
}

#[test]
fn image_credential_read_failure_keeps_repair_pending_without_writing_a_key() {
    use images::credentials::{self, Operation as CredentialOperation};
    let fixture = ImageFixture::new();
    fixture.enable();
    credentials::take_calls();
    credentials::fail_next(CredentialOperation::Read);
    assert_eq!(
        images::repair(&fixture.0.paths, &fixture.program())
            .unwrap_err()
            .code,
        "credential_test_failure"
    );
    let calls = credentials::take_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, CredentialOperation::Read);
    assert_eq!(images::status(&fixture.0.paths).unwrap()["pending"], true);
    assert!(images::snapshot(&fixture.0.paths).is_err());
    assert_eq!(
        images::repair(&fixture.0.paths, &fixture.program()).unwrap()["pending"],
        false
    );
}

#[test]
fn image_credential_delete_failure_attempts_both_slots_and_keeps_cleanup_pending() {
    use images::credentials::{self, Operation as CredentialOperation};
    let fixture = ImageFixture::new();
    fixture.enable();
    let other = ImageFixture::new();
    other.enable();
    let mut provider = fixture.provider();
    provider.model = "replacement-model".into();
    inject("image:state", Action::Error);
    assert!(
        images::enable(
            &fixture.0.paths,
            &fixture.program(),
            provider,
            "replacement-key"
        )
        .is_err()
    );
    let receipt = fixture.0.paths.backup_dir.join("image-provider.json");
    let state = documents::json(&std::fs::read(&receipt).unwrap()).unwrap();
    let service = format!(
        "GitHubAdapter.ImageProvider.{}",
        filesystem::hash(state["codex"].as_str().unwrap().to_lowercase().as_bytes())
    );
    let current = state["key_id"].as_str().unwrap();
    let previous = state["previous_key"].as_str().unwrap();
    assert_ne!(current, previous);
    credentials::take_calls();
    credentials::fail_next(CredentialOperation::Delete);
    assert_eq!(
        images::disable(&fixture.0.paths).unwrap_err().code,
        "credential_test_failure"
    );
    assert_eq!(
        credentials::take_calls(),
        vec![
            (CredentialOperation::Delete, service.clone(), current.into()),
            (
                CredentialOperation::Delete,
                service.clone(),
                previous.into()
            ),
        ]
    );
    assert!(credentials::read(&service, current).unwrap().is_some());
    assert!(credentials::read(&service, previous).unwrap().is_none());
    let pending = documents::json(&std::fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(pending["enabled"], false);
    assert_eq!(pending["pending"], true);
    assert_eq!(pending["key_id"], current);
    assert_eq!(pending["previous_key"], previous);
    assert_eq!(
        images::snapshot(&other.0.paths).unwrap().1,
        "synthetic-image-key"
    );
    assert_eq!(
        images::disable(&fixture.0.paths).unwrap()["configured"],
        false
    );
    assert!(credentials::read(&service, current).unwrap().is_none());
}

#[test]
fn late_skill_edits_remain_durably_owned_conflicts_through_disable() {
    for deleting in [false, true] {
        let fixture = ImageFixture::new();
        fixture.enable();
        let skill = images::skill_path(fixture.0.paths.codex_config.as_ref().unwrap());
        let helper = fixture.0.root.path().join("updated-image-helper.exe");
        std::fs::write(&helper, b"fixture only; never executed").unwrap();
        let edited = b"# independently saved after the final precheck\n";
        inject(
            if deleting {
                "image-skill:before-delete"
            } else {
                "image-skill:before-replace"
            },
            Action::ExternalEdit(skill.clone(), edited.to_vec()),
        );
        let result = if deleting {
            images::disable(&fixture.0.paths)
        } else {
            images::repair(&fixture.0.paths, &helper)
        };
        assert_eq!(result.unwrap_err().code, "configuration_conflict");
        let journal_path = fixture.0.paths.backup_dir.join("image-skill").join(JOURNAL);
        let raw = std::fs::read(&journal_path).unwrap();
        let journal = documents::json(&raw).unwrap();
        assert_eq!(journal["edits"][0]["role"]["kind"], "image_skill");
        let displaced = skill.parent().unwrap().join(format!(
            ".github-adapter-{}-00.displaced",
            journal["id"].as_str().unwrap()
        ));
        assert_eq!(std::fs::read(&displaced).unwrap(), edited);
        // A late replacement may already have captured the edit. Match the existing
        // journal contract: retain the exact bytes and block, never guess a rollback
        // or retire the ownership record underneath the conflict.
        assert_eq!(
            images::disable(&fixture.0.paths).unwrap_err().code,
            "configuration_conflict"
        );
        assert_eq!(std::fs::read(&journal_path).unwrap(), raw);
        assert_eq!(std::fs::read(&displaced).unwrap(), edited);
        let state = documents::json(
            &std::fs::read(fixture.0.paths.backup_dir.join("image-provider.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["enabled"], false);
        assert_eq!(state["pending"], true);
        let service = format!(
            "GitHubAdapter.ImageProvider.{}",
            filesystem::hash(state["codex"].as_str().unwrap().to_lowercase().as_bytes())
        );
        assert!(
            images::credentials::read(&service, state["key_id"].as_str().unwrap())
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn interrupted_skill_publication_and_removal_recover_all_owned_staging() {
    for point in [
        "image-skill:staged",
        "image-skill:replaced",
        "image-skill:before-delete",
    ] {
        let fixture = ImageFixture::new();
        fixture.enable();
        let skill = images::skill_path(fixture.0.paths.codex_config.as_ref().unwrap());
        let helper = fixture.0.root.path().join("updated-image-helper.exe");
        std::fs::write(&helper, b"fixture only; never executed").unwrap();
        inject(point, Action::Error);
        if point == "image-skill:before-delete" {
            assert!(images::disable(&fixture.0.paths).is_err());
        } else {
            assert!(images::repair(&fixture.0.paths, &helper).is_err());
            assert!(
                fixture
                    .0
                    .paths
                    .backup_dir
                    .join("image-skill")
                    .join(JOURNAL)
                    .is_file()
            );
            images::repair(&fixture.0.paths, &helper).unwrap();
        }
        images::disable(&fixture.0.paths).unwrap();
        assert!(!skill.exists());
        assert!(
            !fixture
                .0
                .paths
                .backup_dir
                .join("image-skill")
                .join(JOURNAL)
                .exists()
        );
        assert!(
            !fixture
                .0
                .paths
                .backup_dir
                .join("image-provider.json")
                .exists()
        );
        assert!(
            !snapshot(fixture.0.root.path()).keys().any(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with(".image-") || name.starts_with(".github-adapter-")
            }),
            "owned staging remained after {point}"
        );
    }
}

#[test]
fn image_skill_journals_cannot_redirect_an_edit_or_borrow_a_claude_selection() {
    let fixture = Fixture::new();
    let codex = filesystem::resolve(&fixture.paths, &[Client::Codex]).unwrap();
    let outside = fixture.root.path().join("not-an-owned-skill.md");
    assert!(
        transaction::commit(
            &codex,
            Operation::Patch,
            vec![Edit::new(
                EditKind::ImageSkill,
                outside.clone(),
                None,
                Some(b"x".to_vec())
            )],
            BTreeMap::new()
        )
        .is_err()
    );
    assert!(!outside.exists());
    assert!(!fixture.paths.backup_dir.join(JOURNAL).exists());
    let claude = filesystem::resolve(&fixture.paths, &[Client::Claude]).unwrap();
    assert!(
        transaction::commit(
            &claude,
            Operation::Patch,
            vec![Edit::new(
                EditKind::ImageSkill,
                images::skill_path(fixture.paths.codex_config.as_ref().unwrap()),
                None,
                Some(b"x".to_vec())
            )],
            BTreeMap::new()
        )
        .is_err()
    );
    assert!(!fixture.paths.backup_dir.join(JOURNAL).exists());
}

#[test]
fn an_optional_skill_conflict_cannot_block_restoring_normal_codex_routing() {
    let fixture = ImageFixture::new();
    let options = ConfigureOptions::default();
    let owner =
        on_demand::prepare(&fixture.0.paths, ENDPOINT, &options, std::process::id(), 1).unwrap();
    on_demand::activate(&owner, ENDPOINT, &options).unwrap();
    fixture.enable();
    let skill = images::skill_path(fixture.0.paths.codex_config.as_ref().unwrap());
    let helper = fixture.0.root.path().join("new-image-helper.exe");
    std::fs::write(&helper, b"fixture only; never executed").unwrap();
    inject(
        "image-skill:before-replace",
        Action::ExternalEdit(skill, b"# concurrent user edit\n".to_vec()),
    );
    assert!(images::repair(&fixture.0.paths, &helper).is_err());
    assert!(images::disable(&fixture.0.paths).is_err());
    on_demand::restore(&owner).unwrap();
    let settings = documents::codex_settings(
        filesystem::read(fixture.0.paths.codex_config.as_ref().unwrap())
            .unwrap()
            .as_deref(),
    )
    .unwrap();
    assert_ne!(settings.endpoint.as_deref(), Some(ENDPOINT));
    assert!(on_demand::load(&fixture.0.paths).unwrap().is_none());
}

#[test]
fn malformed_skill_storage_does_not_gate_image_key_revocation() {
    let fixture = ImageFixture::new();
    fixture.enable();
    let receipt = fixture.0.paths.backup_dir.join("image-provider.json");
    let state = documents::json(&std::fs::read(&receipt).unwrap()).unwrap();
    let service = format!(
        "GitHubAdapter.ImageProvider.{}",
        filesystem::hash(state["codex"].as_str().unwrap().to_lowercase().as_bytes())
    );
    let key = state["key_id"].as_str().unwrap();
    let skill_storage = fixture.0.paths.backup_dir.join("image-skill");
    std::fs::remove_dir(&skill_storage).unwrap();
    std::fs::write(&skill_storage, b"conflicting storage fixture").unwrap();
    assert!(images::disable(&fixture.0.paths).is_err());
    assert!(images::credentials::read(&service, key).unwrap().is_none());
    let state = documents::json(&std::fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(state["enabled"], false);
    assert_eq!(state["pending"], true);
    std::fs::remove_file(&skill_storage).unwrap();
    images::disable(&fixture.0.paths).unwrap();
}
