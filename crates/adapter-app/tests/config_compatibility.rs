use adapter_app::config::{self, Client, ClientPaths, ConfigureOptions};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

const ORIGINAL: &[u8] =
    b"# original bytes\r\nmodel = \"fixture-model\"\r\ncustom = \"fixture-secret\"\r\n";
const ENDPOINT: &str = "http://127.0.0.1:5012";

fn paths(root: &Path) -> ClientPaths {
    ClientPaths {
        codex_config: Some(root.join("config.toml")),
        claude_settings: None,
        backup_dir: root.join("backups"),
    }
}

#[test]
fn synthetic_v1_manifest_restores_raw_original_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let file = root.join("config.toml");

    let managed = managed_codex_config(root);
    write_synthetic_v1_backup(root, &managed, ORIGINAL);

    config::restore(&paths(root), &[Client::Codex], false).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), ORIGINAL);
    assert!(!root.join("backups").join("codex.original").exists());
    assert!(!root.join("backups").join("manifest.json").exists());
}

#[test]
fn synthetic_v1_manifest_refuses_modified_current_or_original_backup() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let file = root.join("config.toml");

    let managed = managed_codex_config(root);
    write_synthetic_v1_backup(root, &managed, ORIGINAL);
    std::fs::write(&file, b"model = \"user-edit\"\n").unwrap();
    let error = config::restore(&paths(root), &[Client::Codex], false).unwrap_err();
    assert_eq!(error.status, 409);
    assert!(
        error
            .message
            .contains("Settings changed outside GitHub Adapter")
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"model = \"user-edit\"\n");

    std::fs::write(&file, &managed).unwrap();
    std::fs::write(root.join("backups").join("codex.original"), b"tampered\n").unwrap();
    let error = config::restore(&paths(root), &[Client::Codex], false).unwrap_err();
    assert_eq!(error.status, 409);
    assert!(error.message.contains("original settings backup"));
    assert_eq!(std::fs::read(&file).unwrap(), managed);
}

fn managed_codex_config(root: &Path) -> Vec<u8> {
    let file = root.join("config.toml");
    std::fs::write(&file, ORIGINAL).unwrap();
    config::configure(
        &paths(root),
        ENDPOINT,
        &[Client::Codex],
        &ConfigureOptions {
            model: Some("fixture-model".into()),
            reasoning_effort: Some("high".into()),
            ..ConfigureOptions::default()
        },
    )
    .unwrap();
    let managed = std::fs::read(&file).unwrap();
    std::fs::remove_dir_all(root.join("backups")).unwrap();
    managed
}

fn write_synthetic_v1_backup(root: &Path, managed: &[u8], original: &[u8]) {
    let file = root.join("config.toml");
    let backup = root.join("backups");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("codex.original"), original).unwrap();
    std::fs::write(&file, managed).unwrap();
    let manifest = json!({
        "version": 1,
        "entries": {
            "codex": {
                "target": file.to_str().unwrap(),
                "created": false,
                "original_backup": "codex.original",
                "original_sha256": sha256_hex(original),
                "post_sha256": sha256_hex(managed)
            }
        }
    });
    let mut raw = serde_json::to_vec_pretty(&manifest).unwrap();
    raw.push(b'\n');
    std::fs::write(backup.join("manifest.json"), raw).unwrap();
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
