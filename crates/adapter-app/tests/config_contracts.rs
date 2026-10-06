#![cfg(any(windows, target_os = "macos"))]

use adapter_app::config::{
    Client, ClientPaths, ConfigureOptions, configure, default_paths, inspect, read_codex_settings,
    recover, restore,
};
use serde_json::Value;
#[cfg(windows)]
use std::path::Path;

const BOTH: &[Client] = &[Client::Codex, Client::Claude];
const ENDPOINT: &str = "http://127.0.0.1:5001";

struct Fixture {
    root: tempfile::TempDir,
    paths: ClientPaths,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("native-config-contract-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let paths = ClientPaths {
            codex_config: Some(root.path().join("Codex space").join("config.toml")),
            claude_settings: Some(root.path().join("Claude space").join("settings.json")),
            backup_dir: root.path().join("backups"),
        };
        Self { root, paths }
    }

    fn write(&self, client: Client, value: &[u8]) {
        let path = match client {
            Client::Codex => self.paths.codex_config.as_ref().unwrap(),
            Client::Claude => self.paths.claude_settings.as_ref().unwrap(),
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value).unwrap();
    }

    fn codex(&self) -> Vec<u8> {
        std::fs::read(self.paths.codex_config.as_ref().unwrap()).unwrap()
    }

    fn claude(&self) -> Vec<u8> {
        std::fs::read(self.paths.claude_settings.as_ref().unwrap()).unwrap()
    }
}

#[test]
fn preserves_comments_unrelated_tables_credentials_and_exact_raw_originals() {
    let fixture = Fixture::new();
    let codex = b"# Keep comment\r\nmodel = 'old' # model note\r\nmodel_reasoning_effort = 'ultra' # effort note\r\nopenai_base_url = 'https://old.invalid' # endpoint note\r\n[model_providers.custom]\r\napi_key = 'fixture-only-secret'\r\n[mcp_servers.demo]\r\ncommand = 'keep'\r\n";
    let claude = b"{\"env\":{\"ANTHROPIC_API_KEY\":\"fixture-only-secret\",\"KEEP\":\"yes\"},\"hooks\":{\"Stop\":[]},\"model\":\"keep\"}\r\n";
    fixture.write(Client::Codex, codex);
    fixture.write(Client::Claude, claude);
    let options = ConfigureOptions {
        model: Some("new-model".into()),
        reasoning_effort: Some("max".into()),
        ..Default::default()
    };
    let changes = configure(&fixture.paths, ENDPOINT, BOTH, &options).unwrap();
    assert!(changes.iter().all(|change| change.changed));
    let current = String::from_utf8(fixture.codex()).unwrap();
    for comment in [
        "# Keep comment",
        "# model note",
        "# effort note",
        "# endpoint note",
    ] {
        assert!(current.contains(comment));
    }
    assert!(current.contains("api_key = 'fixture-only-secret'"));
    assert!(current.contains("command = 'keep'"));
    let value: Value = serde_json::from_slice(&fixture.claude()).unwrap();
    assert!(value["env"]["ANTHROPIC_API_KEY"] == "fixture-only-secret");
    assert!(value["model"] == "keep");
    assert!(std::fs::read(fixture.paths.backup_dir.join("codex.original")).unwrap() == codex);
    assert!(std::fs::read(fixture.paths.backup_dir.join("claude.original")).unwrap() == claude);
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(fixture.paths.backup_dir.join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest.as_object().unwrap().len(), 2);
    assert_eq!(manifest["version"], 1);
    for entry in manifest["entries"].as_object().unwrap().values() {
        assert_eq!(entry.as_object().unwrap().len(), 5);
        assert_eq!(entry["created"], false);
    }
    assert!(
        inspect(
            &fixture.paths,
            ENDPOINT,
            BOTH,
            Some("new-model"),
            Some("max")
        )
        .unwrap()
        .iter()
        .all(|status| status.configured && status.managed && status.restorable)
    );
    assert!(!format!("{changes:?}").contains("fixture-only-secret"));
    restore(&fixture.paths, BOTH, false).unwrap();
    assert!(fixture.codex() == codex && fixture.claude() == claude);
    assert_eq!(
        std::fs::read_dir(&fixture.paths.backup_dir)
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn changing_endpoint_models_and_efforts_never_rebaselines_first_original() {
    let fixture = Fixture::new();
    let original = b"model = 'initial'\nmodel_reasoning_effort = 'ultra'\n";
    fixture.write(Client::Codex, original);
    for (endpoint, model, effort) in [
        (ENDPOINT, "first", "max"),
        ("http://127.0.0.1:5012", "second", "high"),
        ("http://[::1]:5012", "second", "max"),
    ] {
        let options = ConfigureOptions {
            model: Some(model.into()),
            reasoning_effort: Some(effort.into()),
            ..Default::default()
        };
        configure(&fixture.paths, endpoint, &[Client::Codex], &options).unwrap();
        let manifest = std::fs::read(fixture.paths.backup_dir.join("manifest.json")).unwrap();
        assert!(
            !configure(&fixture.paths, endpoint, &[Client::Codex], &options).unwrap()[0].changed
        );
        assert!(std::fs::read(fixture.paths.backup_dir.join("manifest.json")).unwrap() == manifest);
        assert!(
            std::fs::read(fixture.paths.backup_dir.join("codex.original")).unwrap() == original
        );
    }
    restore(&fixture.paths, &[Client::Codex], false).unwrap();
    assert!(fixture.codex() == original);
}

#[test]
fn dry_runs_make_no_files_and_created_clients_are_removed_without_removing_parents() {
    let fixture = Fixture::new();
    assert!(
        configure(
            &fixture.paths,
            ENDPOINT,
            BOTH,
            &ConfigureOptions {
                dry_run: true,
                ..Default::default()
            }
        )
        .unwrap()
        .iter()
        .all(|change| change.changed)
    );
    assert_eq!(std::fs::read_dir(fixture.root.path()).unwrap().count(), 0);
    assert!(recover(&fixture.paths, true).unwrap().is_empty());
    configure(&fixture.paths, ENDPOINT, BOTH, &ConfigureOptions::default()).unwrap();
    let codex = fixture.codex();
    restore(&fixture.paths, BOTH, true).unwrap();
    assert!(fixture.codex() == codex);
    let parent = fixture
        .paths
        .codex_config
        .as_ref()
        .unwrap()
        .parent()
        .unwrap();
    std::fs::write(parent.join("unrelated"), b"keep").unwrap();
    restore(&fixture.paths, BOTH, false).unwrap();
    assert!(!fixture.paths.codex_config.as_ref().unwrap().exists());
    assert!(!fixture.paths.claude_settings.as_ref().unwrap().exists());
    assert!(parent.join("unrelated").exists());
    assert!(
        fixture
            .paths
            .claude_settings
            .as_ref()
            .unwrap()
            .parent()
            .unwrap()
            .is_dir()
    );
}

#[test]
fn already_configured_unmanaged_files_do_not_acquire_fake_originals() {
    let fixture = Fixture::new();
    let original = format!("model = 'keep'\nopenai_base_url = '{ENDPOINT}' # exact\n").into_bytes();
    fixture.write(Client::Codex, &original);
    assert!(
        !configure(
            &fixture.paths,
            ENDPOINT,
            &[Client::Codex],
            &ConfigureOptions::default()
        )
        .unwrap()[0]
            .changed
    );
    assert!(!fixture.paths.backup_dir.exists());
    let status = inspect(&fixture.paths, ENDPOINT, &[Client::Codex], None, None)
        .unwrap()
        .remove(0);
    assert!(status.configured && !status.managed && !status.restorable);
    assert!(!restore(&fixture.paths, &[Client::Codex], false).unwrap()[0].changed);
    assert!(fixture.codex() == original);
}

#[test]
fn explicit_force_preserves_custom_tables_and_inspection_detects_effective_overrides() {
    let fixture = Fixture::new();
    let original =
        b"model_provider = 'custom'\n[model_providers.custom]\napi_key = 'fixture-only-secret'\n";
    fixture.write(Client::Codex, original);
    assert!(
        configure(
            &fixture.paths,
            ENDPOINT,
            &[Client::Codex],
            &ConfigureOptions::default()
        )
        .is_err()
    );
    assert!(fixture.codex() == original);
    configure(
        &fixture.paths,
        ENDPOINT,
        &[Client::Codex],
        &ConfigureOptions {
            force_openai_provider: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        read_codex_settings(&fixture.paths).unwrap().provider,
        "openai"
    );
    assert!(
        String::from_utf8(fixture.codex())
            .unwrap()
            .contains("api_key = 'fixture-only-secret'")
    );
    restore(&fixture.paths, &[Client::Codex], false).unwrap();
    for raw in [
        b"[model_providers.openai]\nbase_url = 'https://fixture-secret.invalid'\n".as_slice(),
        b"profile = 'active'\n[profiles.active]\nmodel = 'different'\n",
        b"model_providers = { openai = {} }\n",
    ] {
        fixture.write(Client::Codex, raw);
        configure(
            &fixture.paths,
            ENDPOINT,
            &[Client::Codex],
            &ConfigureOptions::default(),
        )
        .unwrap();
        let status = inspect(&fixture.paths, ENDPOINT, &[Client::Codex], None, None)
            .unwrap()
            .remove(0);
        assert!(!status.configured && !status.issues.is_empty());
        assert!(!format!("{status:?}").contains("fixture-secret"));
        assert!(
            !format!("{:?}", read_codex_settings(&fixture.paths).unwrap())
                .contains("fixture-secret")
        );
        restore(&fixture.paths, &[Client::Codex], false).unwrap();
        assert!(fixture.codex() == raw);
    }
}

#[test]
fn malformed_selected_files_invalid_metadata_and_duplicate_clients_fail_without_writes() {
    let fixture = Fixture::new();
    for raw in [
        b"model = [".as_slice(),
        b"\xff",
        b"model_provider = false\n",
    ] {
        fixture.write(Client::Codex, raw);
        assert!(configure(&fixture.paths, ENDPOINT, BOTH, &ConfigureOptions::default()).is_err());
        assert!(!fixture.paths.backup_dir.exists());
        assert!(fixture.codex() == raw);
    }
    fixture.write(Client::Codex, b"model = 'keep'\n");
    for raw in [
        b"[]".as_slice(),
        b"{\"env\":null}",
        b"{\"env\":[]}",
        b"{\"x\":1,\"x\":2}",
        b"{\"x\":1e999}",
    ] {
        fixture.write(Client::Claude, raw);
        assert!(configure(&fixture.paths, ENDPOINT, BOTH, &ConfigureOptions::default()).is_err());
        assert!(!fixture.paths.backup_dir.exists());
        assert!(fixture.claude() == raw);
    }
    for raw in [
        b"model_context_window = -1\n".as_slice(),
        b"profile = 'absent'\n",
        b"model_providers = { openai = 'fixture-only-secret' }\n",
        b"model = true\n",
    ] {
        fixture.write(Client::Codex, raw);
        let error = read_codex_settings(&fixture.paths).unwrap_err();
        assert!(!error.message.contains("fixture-only-secret"));
    }
    for clients in [vec![], vec![Client::Codex, Client::Codex]] {
        assert!(
            configure(
                &fixture.paths,
                ENDPOINT,
                &clients,
                &ConfigureOptions::default()
            )
            .is_err()
        );
        assert!(restore(&fixture.paths, &clients, false).is_err());
        assert!(inspect(&fixture.paths, ENDPOINT, &clients, None, None).is_err());
    }
    assert!(
        configure(
            &fixture.paths,
            ENDPOINT,
            &[Client::Claude],
            &ConfigureOptions {
                model: Some("x".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        inspect(
            &fixture.paths,
            ENDPOINT,
            &[Client::Codex],
            None,
            Some("high")
        )
        .is_err()
    );
}

#[test]
fn external_changes_modified_originals_and_manifest_redirection_are_preserved() {
    let fixture = Fixture::new();
    fixture.write(Client::Codex, b"model = 'old'\n");
    configure(
        &fixture.paths,
        ENDPOINT,
        &[Client::Codex],
        &ConfigureOptions::default(),
    )
    .unwrap();
    let modified = [
        fixture.codex(),
        b"# external fixture-only-secret\n".to_vec(),
    ]
    .concat();
    fixture.write(Client::Codex, &modified);
    let manifest = std::fs::read(fixture.paths.backup_dir.join("manifest.json")).unwrap();
    assert!(
        configure(
            &fixture.paths,
            "http://127.0.0.1:5012",
            &[Client::Codex],
            &ConfigureOptions::default()
        )
        .is_err()
    );
    assert!(restore(&fixture.paths, &[Client::Codex], false).is_err());
    assert!(fixture.codex() == modified);
    assert!(std::fs::read(fixture.paths.backup_dir.join("manifest.json")).unwrap() == manifest);
    let status = inspect(&fixture.paths, ENDPOINT, &[Client::Codex], None, None)
        .unwrap()
        .remove(0);
    assert!(status.configured && status.managed && !status.restorable);
    assert!(!format!("{status:?}").contains("fixture-only-secret"));
}

#[test]
fn selected_paths_reject_overlap_missing_targets_directories_reparse_points_and_oversize_files() {
    let fixture = Fixture::new();
    let missing = ClientPaths {
        codex_config: None,
        claude_settings: None,
        backup_dir: fixture.paths.backup_dir.clone(),
    };
    assert!(
        configure(
            &missing,
            ENDPOINT,
            &[Client::Codex],
            &ConfigureOptions::default()
        )
        .is_err()
    );
    let overlap = ClientPaths {
        claude_settings: fixture.paths.codex_config.clone(),
        ..fixture.paths.clone()
    };
    assert!(inspect(&overlap, ENDPOINT, BOTH, None, None).is_err());
    let inside = ClientPaths {
        codex_config: Some(fixture.paths.backup_dir.join("config.toml")),
        ..fixture.paths.clone()
    };
    assert!(
        configure(
            &inside,
            ENDPOINT,
            &[Client::Codex],
            &ConfigureOptions::default()
        )
        .is_err()
    );
    std::fs::create_dir_all(fixture.paths.codex_config.as_ref().unwrap()).unwrap();
    assert!(read_codex_settings(&fixture.paths).is_err());
    std::fs::remove_dir(fixture.paths.codex_config.as_ref().unwrap()).unwrap();
    fixture.write(Client::Codex, &vec![b'#'; 8 * 1024 * 1024 + 1]);
    assert!(read_codex_settings(&fixture.paths).is_err());
    let junction = fixture.root.path().join("junction");
    let real = fixture
        .paths
        .codex_config
        .as_ref()
        .unwrap()
        .parent()
        .unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(real)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Could not create an isolated fixture junction."
        );
    }
    #[cfg(target_os = "macos")]
    std::os::unix::fs::symlink(real, &junction).unwrap();
    let reparse = ClientPaths {
        codex_config: Some(junction.join("config.toml")),
        ..fixture.paths.clone()
    };
    assert!(read_codex_settings(&reparse).is_err());
    assert!(!fixture.paths.backup_dir.exists());
}

#[test]
fn explicit_paths_keep_unselected_overrides_without_resolving_them() {
    let fixture = Fixture::new();
    let unused = fixture
        .root
        .path()
        .join("unused-invalid-directory")
        .join("unselected");
    let paths = default_paths(
        fixture.paths.codex_config.clone(),
        Some(unused.clone()),
        Some(fixture.paths.backup_dir.clone()),
        &[Client::Codex],
    )
    .unwrap();
    assert_eq!(paths.claude_settings, Some(unused));
    configure(
        &paths,
        ENDPOINT,
        &[Client::Codex],
        &ConfigureOptions::default(),
    )
    .unwrap();
    assert!(!fixture.paths.claude_settings.as_ref().unwrap().exists());
}

#[cfg(windows)]
fn dacl(path: &Path) -> (u16, Vec<u8>) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetFileSecurityW, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut size = 0;
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut size,
        )
    };
    assert!(size > 0);
    let mut storage = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                storage.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        },
        0
    );
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
    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe {
            GetSecurityDescriptorControl(storage.as_mut_ptr().cast(), &mut control, &mut revision)
        },
        0
    );
    (control, unsafe {
        std::slice::from_raw_parts(acl.cast::<u8>(), (*acl).AclSize as usize).to_vec()
    })
}

#[test]
#[cfg(windows)]
fn native_replace_preserves_existing_dacl_for_configure_update_and_restore() {
    let fixture = Fixture::new();
    fixture.write(Client::Codex, b"model = 'original'\n");
    let path = fixture.paths.codex_config.as_ref().unwrap();
    let (original_control, original_dacl) = dacl(path);
    let (repeated_control, repeated_dacl) = dacl(path);
    assert_eq!(
        repeated_dacl, original_dacl,
        "An unchanged fixture DACL returned different raw bytes; control {original_control:#06x} -> {repeated_control:#06x}."
    );
    configure(
        &fixture.paths,
        ENDPOINT,
        &[Client::Codex],
        &ConfigureOptions::default(),
    )
    .unwrap();
    let (control, configured_dacl) = dacl(path);
    assert_eq!(
        configured_dacl, original_dacl,
        "ReplaceFileW changed the existing DACL on configure; control {original_control:#06x} -> {control:#06x}."
    );
    configure(
        &fixture.paths,
        "http://127.0.0.1:5012",
        &[Client::Codex],
        &ConfigureOptions::default(),
    )
    .unwrap();
    let (control, updated_dacl) = dacl(path);
    assert_eq!(
        updated_dacl, original_dacl,
        "ReplaceFileW changed the existing DACL on update; control {original_control:#06x} -> {control:#06x}."
    );
    restore(&fixture.paths, &[Client::Codex], false).unwrap();
    let (control, restored_dacl) = dacl(path);
    assert_eq!(
        restored_dacl, original_dacl,
        "ReplaceFileW changed the existing DACL on restore; control {original_control:#06x} -> {control:#06x}."
    );
}
