//! Selected-client configuration with Python-compatible originals and native recovery.

mod documents;
mod filesystem;
pub mod images;
pub mod on_demand;
mod transaction;

#[cfg(all(test, any(windows, target_os = "macos")))]
mod tests;

use adapter_protocol::{AdapterError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use documents::ManifestEntry;
use transaction::{Edit, EditKind, Operation};

pub(super) const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MANIFEST: &str = "manifest.json";
pub(super) const JOURNAL: &str = "native-configuration-journal.json";

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Client {
    Codex,
    Claude,
}

impl Client {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub(super) fn original(self) -> String {
        format!("{}.original", self.name())
    }
}

#[derive(Clone, Debug)]
pub struct ClientPaths {
    pub codex_config: Option<PathBuf>,
    pub claude_settings: Option<PathBuf>,
    pub backup_dir: PathBuf,
}

impl ClientPaths {
    pub(super) fn path(&self, client: Client) -> Option<&Path> {
        match client {
            Client::Codex => self.codex_config.as_deref(),
            Client::Claude => self.claude_settings.as_deref(),
        }
    }
}

#[derive(Clone, Default)]
pub struct ConfigureOptions {
    pub dry_run: bool,
    pub force_openai_provider: bool,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ConfigurationChange {
    pub client: Client,
    pub path: PathBuf,
    pub changed: bool,
    pub backup: Option<PathBuf>,
    pub action: String,
}

#[derive(Clone, Debug)]
pub struct ConfigurationStatus {
    pub client: Client,
    pub path: PathBuf,
    pub configured: bool,
    pub managed: bool,
    pub restorable: bool,
    pub issues: Vec<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct CodexSettings {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub provider: String,
    pub endpoint: Option<String>,
    pub profile_overrides: Vec<String>,
    pub context_window: Option<u64>,
    pub auto_compact_token_limit: Option<u64>,
    pub openai_provider_override: bool,
}

impl fmt::Debug for CodexSettings {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output
            .debug_struct("CodexSettings")
            .field("has_model", &self.model.is_some())
            .field("has_reasoning_effort", &self.reasoning_effort.is_some())
            .field("uses_openai", &(self.provider == "openai"))
            .field("has_endpoint", &self.endpoint.is_some())
            .field("profile_overrides", &self.profile_overrides)
            .field("context_window", &self.context_window)
            .field("auto_compact_token_limit", &self.auto_compact_token_limit)
            .field("openai_provider_override", &self.openai_provider_override)
            .finish()
    }
}

pub fn default_paths(
    codex: Option<PathBuf>,
    claude: Option<PathBuf>,
    backup: Option<PathBuf>,
    clients: &[Client],
) -> Result<ClientPaths> {
    validate_clients(clients)?;
    let codex_config = match codex {
        None if clients.contains(&Client::Codex) => {
            Some(environment_directory("CODEX_HOME", &[".codex"])?.join("config.toml"))
        }
        supplied => supplied,
    };
    let claude_settings = match claude {
        None if clients.contains(&Client::Claude) => {
            Some(environment_directory("CLAUDE_CONFIG_DIR", &[".claude"])?.join("settings.json"))
        }
        supplied => supplied,
    };
    let backup_dir = match backup {
        Some(path) => path,
        None => environment_directory(
            if cfg!(windows) {
                "LOCALAPPDATA"
            } else {
                "XDG_STATE_HOME"
            },
            if cfg!(target_os = "macos") {
                &["Library", "Application Support"]
            } else {
                &[".local", "state"]
            },
        )?
        .join("GitHubAdapter")
        .join("client-backups"),
    };
    Ok(ClientPaths {
        codex_config,
        claude_settings,
        backup_dir,
    })
}

fn environment_directory(name: &str, fallback: &[&str]) -> Result<PathBuf> {
    if let Some(value) = std::env::var_os(name) {
        if value.to_string_lossy().trim().is_empty() {
            return Err(invalid(
                "A selected client/state directory environment override is empty.",
            ));
        }
        return Ok(PathBuf::from(value));
    }
    let mut path = home()?;
    for part in fallback {
        path.push(part);
    }
    Ok(path)
}

pub(super) fn home() -> Result<PathBuf> {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(name)
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            invalid("Cannot determine the home directory; supply explicit selected paths.")
        })
}

pub fn configure(
    paths: &ClientPaths,
    endpoint: &str,
    clients: &[Client],
    options: &ConfigureOptions,
) -> Result<Vec<ConfigurationChange>> {
    configure_inner(paths, endpoint, clients, options, false)
}

/// Reuse compatible live settings without rewriting them or adopting external edits.
pub fn ensure_configured(
    paths: &ClientPaths,
    endpoint: &str,
    clients: &[Client],
    options: &ConfigureOptions,
) -> Result<Vec<ConfigurationChange>> {
    configure_inner(paths, endpoint, clients, options, true)
}

fn configure_inner(
    paths: &ClientPaths,
    endpoint: &str,
    clients: &[Client],
    options: &ConfigureOptions,
    reuse_compatible: bool,
) -> Result<Vec<ConfigurationChange>> {
    let endpoint = documents::endpoint(endpoint)?;
    validate_selection(
        clients,
        options.model.as_deref(),
        options.reasoning_effort.as_deref(),
    )?;
    let resolved = filesystem::resolve(paths, clients)?;
    let _locks = transaction::lock(paths, &resolved)?;
    let mut changes = transaction::resume_before_write(paths, &resolved, options.dry_run)?;
    let manifest_path = resolved.backup.join(MANIFEST);
    let manifest_before = filesystem::read(&manifest_path)?;
    let mut manifest = documents::manifest(manifest_before.as_deref())?;
    let mut reads = BTreeMap::from([(manifest_path.clone(), manifest_before.clone())]);
    let mut backups = Vec::new();
    let mut edits = Vec::new();
    for client in clients {
        let path = &resolved.targets[client];
        let before = filesystem::read(path)?;
        reads.insert(path.clone(), before.clone());
        let reuse = reuse_compatible
            && documents::configured(
                *client,
                before.as_deref(),
                &endpoint,
                options.model.as_deref(),
                options.reasoning_effort.as_deref(),
            )?
            .0;
        let entry = manifest.entries.get(client);
        let mut backup = None;
        if let Some(entry) = entry {
            let (_, original_path) = original(
                *client,
                path,
                before.as_deref(),
                entry,
                &resolved.backup,
                reuse,
            )?;
            backup = original_path;
            if let Some(path) = &backup {
                reads.insert(path.clone(), filesystem::read(path)?);
            }
        }
        if reuse {
            changes.push(ConfigurationChange {
                client: *client,
                path: path.clone(),
                changed: false,
                backup,
                action: "reuse compatible settings".into(),
            });
            continue;
        }
        let after = documents::render(*client, before.as_deref(), &endpoint, options)?;
        let changed = before.as_deref() != Some(after.as_slice());
        if changed {
            let updated = if let Some(entry) = entry {
                ManifestEntry {
                    post_sha256: filesystem::hash(&after),
                    ..entry.clone()
                }
            } else {
                let original_path = resolved.backup.join(client.original());
                let existing = filesystem::read(&original_path)?;
                if existing.is_some() {
                    return Err(conflict(
                        "An unclaimed original backup exists; refusing to overwrite it.",
                    ));
                }
                reads.insert(original_path.clone(), existing);
                if let Some(raw) = &before {
                    backup = Some(original_path.clone());
                    backups.push(Edit::new(
                        EditKind::Original(*client),
                        original_path,
                        None,
                        Some(raw.clone()),
                    ));
                }
                ManifestEntry {
                    target: filesystem::path_text(path)?,
                    created: before.is_none(),
                    original_backup: backup.as_ref().map(|_| client.original()),
                    original_sha256: before.as_deref().map(filesystem::hash),
                    post_sha256: filesystem::hash(&after),
                }
            };
            manifest.entries.insert(*client, updated);
            edits.push(Edit::new(
                EditKind::Target(*client),
                path.clone(),
                before,
                Some(after),
            ));
        }
        changes.push(ConfigurationChange {
            client: *client,
            path: path.clone(),
            changed,
            backup,
            action: if changed { "configure" } else { "unchanged" }.into(),
        });
    }
    if !edits.is_empty() && !options.dry_run {
        backups.extend(edits);
        backups.push(Edit::new(
            EditKind::Manifest,
            manifest_path,
            manifest_before,
            Some(documents::serialize(&manifest)?),
        ));
        transaction::commit(&resolved, Operation::Configure, backups, reads)?;
    }
    Ok(changes)
}

pub fn restore(
    paths: &ClientPaths,
    clients: &[Client],
    dry_run: bool,
) -> Result<Vec<ConfigurationChange>> {
    validate_clients(clients)?;
    let resolved = filesystem::resolve(paths, clients)?;
    let _locks = transaction::lock(paths, &resolved)?;
    let mut changes = transaction::resume_before_write(paths, &resolved, dry_run)?;
    let manifest_path = resolved.backup.join(MANIFEST);
    let manifest_before = filesystem::read(&manifest_path)?;
    let mut manifest = documents::manifest(manifest_before.as_deref())?;
    let mut reads = BTreeMap::from([(manifest_path.clone(), manifest_before.clone())]);
    let mut edits = Vec::new();
    let mut cleanup = Vec::new();
    for client in clients {
        let path = &resolved.targets[client];
        let before = filesystem::read(path)?;
        documents::validate_document(*client, before.as_deref())?;
        reads.insert(path.clone(), before.clone());
        let Some(entry) = manifest.entries.get(client) else {
            changes.push(ConfigurationChange {
                client: *client,
                path: path.clone(),
                changed: false,
                backup: None,
                action: "not managed".into(),
            });
            continue;
        };
        let (after, backup) = original(
            *client,
            path,
            before.as_deref(),
            entry,
            &resolved.backup,
            false,
        )?;
        edits.push(Edit::new(
            EditKind::Target(*client),
            path.clone(),
            before,
            after.clone(),
        ));
        if let Some(backup) = &backup {
            reads.insert(backup.clone(), after.clone());
            cleanup.push(Edit::new(
                EditKind::Original(*client),
                backup.clone(),
                after,
                None,
            ));
        }
        manifest.entries.remove(client);
        changes.push(ConfigurationChange {
            client: *client,
            path: path.clone(),
            changed: true,
            backup,
            action: "restore".into(),
        });
    }
    if !edits.is_empty() && !dry_run {
        let after = if manifest.entries.is_empty() {
            None
        } else {
            Some(documents::serialize(&manifest)?)
        };
        edits.push(Edit::new(
            EditKind::Manifest,
            manifest_path,
            manifest_before,
            after,
        ));
        edits.extend(cleanup);
        transaction::commit(&resolved, Operation::Restore, edits, reads)?;
    }
    Ok(changes)
}

pub fn inspect(
    paths: &ClientPaths,
    endpoint: &str,
    clients: &[Client],
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Vec<ConfigurationStatus>> {
    let endpoint = documents::endpoint(endpoint)?;
    validate_selection(clients, model, effort)?;
    let resolved = filesystem::resolve(paths, clients)?;
    let _locks = transaction::lock(paths, &resolved)?;
    transaction::require_no_journal(&resolved.backup)?;
    let manifest =
        documents::manifest(filesystem::read(&resolved.backup.join(MANIFEST))?.as_deref())?;
    let mut statuses = Vec::new();
    for client in clients {
        let path = &resolved.targets[client];
        let current = filesystem::read(path)?;
        let (configured, mut issues) =
            documents::configured(*client, current.as_deref(), &endpoint, model, effort)?;
        let entry = manifest.entries.get(client);
        let restorable = if let Some(entry) = entry {
            match original(
                *client,
                path,
                current.as_deref(),
                entry,
                &resolved.backup,
                false,
            ) {
                Ok(_) => true,
                Err(error) => {
                    issues.push(error.message);
                    false
                }
            }
        } else {
            false
        };
        statuses.push(ConfigurationStatus {
            client: *client,
            path: path.clone(),
            configured,
            managed: entry.is_some(),
            restorable,
            issues,
        });
    }
    Ok(statuses)
}

pub fn read_codex_settings(paths: &ClientPaths) -> Result<CodexSettings> {
    let resolved = filesystem::resolve(paths, &[Client::Codex])?;
    let _locks = transaction::lock(paths, &resolved)?;
    transaction::require_no_journal(&resolved.backup)?;
    documents::codex_settings(filesystem::read(&resolved.targets[&Client::Codex])?.as_deref())
}

pub fn recover(paths: &ClientPaths, dry_run: bool) -> Result<Vec<ConfigurationChange>> {
    let clients: Vec<_> = [Client::Codex, Client::Claude]
        .into_iter()
        .filter(|client| paths.path(*client).is_some())
        .collect();
    let resolved = if clients.is_empty() {
        filesystem::Resolved {
            targets: BTreeMap::new(),
            backup: filesystem::absolute(&paths.backup_dir, true)?,
        }
    } else {
        filesystem::resolve(paths, &clients)?
    };
    let _locks = transaction::lock(paths, &resolved)?;
    transaction::recover_locked(paths, &resolved, dry_run)
}

fn original(
    client: Client,
    target: &Path,
    current: Option<&[u8]>,
    entry: &ManifestEntry,
    backup_dir: &Path,
    reuse_compatible: bool,
) -> Result<(Option<Vec<u8>>, Option<PathBuf>)> {
    let saved = filesystem::absolute(Path::new(&entry.target), false)?;
    if filesystem::key(target)? != filesystem::key(&saved)? {
        return Err(conflict(
            "The saved backup belongs to a different selected settings path.",
        ));
    }
    let backup = (!entry.created).then(|| backup_dir.join(client.original()));
    let before = if let Some(path) = &backup {
        let bytes = filesystem::read(path)?;
        if bytes.as_deref().map(filesystem::hash) != entry.original_sha256 {
            return Err(conflict(
                "An original settings backup is missing or modified; restore is blocked.",
            ));
        }
        documents::validate_document(client, bytes.as_deref())?;
        bytes
    } else {
        None
    };
    if !reuse_compatible
        && current.map(filesystem::hash).as_deref() != Some(entry.post_sha256.as_str())
    {
        return Err(conflict(
            "Settings changed outside GitHub Adapter; automatic setup/restore is blocked to preserve your edits.",
        ));
    }
    Ok((before, backup))
}

fn validate_clients(clients: &[Client]) -> Result<()> {
    if clients.is_empty()
        || clients
            .iter()
            .enumerate()
            .any(|(index, client)| clients[..index].contains(client))
    {
        return Err(invalid("Select codex, claude, or both once each."));
    }
    Ok(())
}

fn validate_selection(clients: &[Client], model: Option<&str>, effort: Option<&str>) -> Result<()> {
    validate_clients(clients)?;
    if model.is_some_and(|value| value.trim().is_empty())
        || effort.is_some_and(|value| value.trim().is_empty())
    {
        return Err(invalid(
            "Explicit model and reasoning selections must be nonempty strings.",
        ));
    }
    if (model.is_some() || effort.is_some()) && !clients.contains(&Client::Codex) {
        return Err(invalid(
            "Model and reasoning selections require the codex client.",
        ));
    }
    if effort.is_some() && model.is_none() {
        return Err(invalid(
            "A reasoning selection requires an explicit model selection.",
        ));
    }
    Ok(())
}

pub(super) fn invalid(message: &str) -> AdapterError {
    AdapterError::new(400, "configuration_error", message)
}

pub(super) fn conflict(message: &str) -> AdapterError {
    AdapterError::new(409, "configuration_conflict", message)
}

pub(super) fn io_error(operation: &str, error: &std::io::Error) -> AdapterError {
    AdapterError::new(
        500,
        "configuration_io_error",
        format!(
            "Could not {operation}; any published recovery journal was preserved (OS error {}).",
            error.raw_os_error().unwrap_or(0)
        ),
    )
}

#[cfg(any(not(test), not(any(windows, target_os = "macos"))))]
pub(super) fn checkpoint(_: &str) -> Result<()> {
    Ok(())
}

#[cfg(all(test, any(windows, target_os = "macos")))]
pub(super) fn checkpoint(label: &str) -> Result<()> {
    tests::checkpoint(label)
}
