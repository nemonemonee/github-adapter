//! Temporary Codex routing, backed by the existing atomic configuration journal.
//! Persistent setup and its first-original backup remain separate and untouched.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

const OWNER: &str = "owner.json";

#[cfg(target_os = "macos")]
pub(crate) fn update_login_registration(
    path: &Path,
    expected_bytes: &[u8],
    arm: bool,
) -> Result<()> {
    filesystem::update_login_registration(path, expected_bytes, arm)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub version: u32,
    pub id: String,
    pub pid: u32,
    pub process_created: u64,
    pub target: String,
    pub backup_dir: String,
    pub applied: BTreeMap<String, Option<String>>,
}

pub fn mode_paths(base: &ClientPaths) -> ClientPaths {
    ClientPaths {
        codex_config: base.codex_config.clone(),
        claude_settings: None,
        backup_dir: base.backup_dir.join("on-demand"),
    }
}

impl Owner {
    pub fn paths(&self) -> ClientPaths {
        ClientPaths {
            codex_config: Some(PathBuf::from(&self.target)),
            claude_settings: None,
            backup_dir: PathBuf::from(&self.backup_dir),
        }
    }
}

pub fn load(base: &ClientPaths) -> Result<Option<Owner>> {
    let selected = mode_paths(base);
    let resolved = filesystem::resolve(&selected, &[Client::Codex])?;
    let Some(raw) = filesystem::read(&resolved.backup.join(OWNER))? else {
        return Ok(None);
    };
    let value = documents::json(&raw)?;
    let owner: Owner = serde_json::from_value(value)
        .map_err(|_| invalid("The on-demand routing receipt is invalid."))?;
    let base_resolved = filesystem::resolve(base, &[Client::Codex])?;
    if owner.version != 1
        || !documents::valid_hash(&owner.id)
        || owner.applied.len() != documents::ROUTE_KEYS.len()
        || documents::ROUTE_KEYS
            .iter()
            .any(|key| !owner.applied.contains_key(*key))
        || filesystem::key(Path::new(&owner.target))?
            != filesystem::key(&resolved.targets[&Client::Codex])?
        || filesystem::key(Path::new(&owner.backup_dir))? != filesystem::key(&base_resolved.backup)?
    {
        return Err(conflict(
            "The on-demand receipt does not match the selected settings paths.",
        ));
    }
    Ok(Some(owner))
}

pub fn prepare(
    base: &ClientPaths,
    endpoint: &str,
    options: &ConfigureOptions,
    pid: u32,
    process_created: u64,
) -> Result<Owner> {
    plan(base, endpoint, options, pid, process_created)?.publish()
}

pub(crate) struct Preparation {
    owner: Owner,
    resolved: filesystem::Resolved,
    _locks: transaction::Locks,
}

impl Preparation {
    pub(crate) fn owner(&self) -> &Owner {
        &self.owner
    }

    pub(crate) fn publish(self) -> Result<Owner> {
        if self.resolved.backup.exists() {
            filesystem::check(&self.resolved.backup, true)?;
        } else {
            filesystem::private_directory(&self.resolved.backup)?;
        }
        filesystem::write_private(
            &self.resolved.backup.join(OWNER),
            &documents::serialize(&self.owner)?,
        )?;
        Ok(self.owner)
    }
}

pub(crate) fn plan(
    base: &ClientPaths,
    endpoint: &str,
    options: &ConfigureOptions,
    pid: u32,
    process_created: u64,
) -> Result<Preparation> {
    if options.dry_run {
        return Err(invalid("An on-demand activation cannot be a dry run."));
    }
    let selected = mode_paths(base);
    let resolved = filesystem::resolve(&selected, &[Client::Codex])?;
    let _locks = transaction::lock(&selected, &resolved)?;
    transaction::resume_before_write(&selected, &resolved, false)?;
    if filesystem::read(&resolved.backup.join(OWNER))?.is_some()
        || filesystem::read(&resolved.backup.join(MANIFEST))?.is_some()
    {
        return Err(conflict(
            "An existing on-demand activation must finish recovery before another starts.",
        ));
    }
    let current = filesystem::read(&resolved.targets[&Client::Codex])?;
    if documents::codex_settings(current.as_deref())?
        .endpoint
        .as_deref()
        == Some(endpoint)
    {
        let legacy = filesystem::resolve(base, &[Client::Codex])?;
        let manifest =
            documents::manifest(filesystem::read(&legacy.backup.join(MANIFEST))?.as_deref())?;
        let entry = manifest.entries.get(&Client::Codex).ok_or_else(|| {
            conflict(
                "A local adapter route has no verified original; normal routing cannot be guessed.",
            )
        })?;
        let (saved, _) = original(
            Client::Codex,
            &legacy.targets[&Client::Codex],
            current.as_deref(),
            entry,
            &legacy.backup,
            true,
        )?;
        if documents::codex_settings(saved.as_deref())?
            .endpoint
            .as_deref()
            == Some(endpoint)
        {
            return Err(conflict("The saved original also depends on this adapter."));
        }
    }
    let after = documents::render(Client::Codex, current.as_deref(), endpoint, options)?;
    let base_resolved = filesystem::resolve(base, &[Client::Codex])?;
    let id = filesystem::hash(
        format!(
            "{pid}:{process_created}:{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        )
        .as_bytes(),
    );
    let owner = Owner {
        version: 1,
        id,
        pid,
        process_created,
        target: filesystem::path_text(&resolved.targets[&Client::Codex])?,
        backup_dir: filesystem::path_text(&base_resolved.backup)?,
        applied: documents::route_fields(Some(&after))?,
    };
    Ok(Preparation {
        owner,
        resolved,
        _locks,
    })
}

/// Migrate a verified old persistent adapter route back to the user's normal
/// routing fields. Never replace the first-original backup or unrelated edits.
pub fn restore_legacy_route(
    base: &ClientPaths,
    endpoint: &str,
) -> Result<Vec<ConfigurationChange>> {
    restore_legacy_fields(base, endpoint, None)
}

fn restore_legacy_fields(
    base: &ClientPaths,
    endpoint: &str,
    applied: Option<&BTreeMap<String, Option<String>>>,
) -> Result<Vec<ConfigurationChange>> {
    let resolved = filesystem::resolve(base, &[Client::Codex])?;
    let _locks = transaction::lock(base, &resolved)?;
    let mut changes = transaction::resume_before_write(base, &resolved, false)?;
    let target = &resolved.targets[&Client::Codex];
    let current = filesystem::read(target)?;
    if documents::codex_settings(current.as_deref())?
        .endpoint
        .as_deref()
        != Some(endpoint)
    {
        return Ok(changes);
    }
    let manifest_path = resolved.backup.join(MANIFEST);
    let manifest_raw = filesystem::read(&manifest_path)?;
    let manifest = documents::manifest(manifest_raw.as_deref())?;
    let entry = manifest.entries.get(&Client::Codex).ok_or_else(|| conflict(
        "Codex already points at this local adapter without a verified original backup. Normal routing cannot be guessed."))?;
    let (saved, backup) = original(
        Client::Codex,
        target,
        current.as_deref(),
        entry,
        &resolved.backup,
        true,
    )?;
    let after = if applied.is_none()
        && current
            .as_deref()
            .is_some_and(|raw| filesystem::hash(raw) == entry.post_sha256)
    {
        // No external edits: retain original bytes, including CRLF and absence.
        saved.clone()
    } else {
        documents::restore_route_fields(current.as_deref(), saved.as_deref(), applied)?
    };
    if documents::codex_settings(after.as_deref())?
        .endpoint
        .as_deref()
        == Some(endpoint)
    {
        return Err(conflict(
            "The saved original also points at this adapter; it is not an independent normal configuration.",
        ));
    }
    if after != current {
        let mut reads = BTreeMap::from([
            (target.clone(), current.clone()),
            (manifest_path, manifest_raw),
        ]);
        if let Some(path) = &backup {
            reads.insert(path.clone(), saved);
        }
        transaction::commit(
            &resolved,
            Operation::Patch,
            vec![Edit::new(
                EditKind::Target(Client::Codex),
                target.clone(),
                current,
                after,
            )],
            reads,
        )?;
        changes.push(ConfigurationChange {
            client: Client::Codex,
            path: target.clone(),
            changed: true,
            backup,
            action: "restore normal routing; preserve other settings".into(),
        });
    }
    Ok(changes)
}

pub fn activate(
    owner: &Owner,
    endpoint: &str,
    options: &ConfigureOptions,
) -> Result<Vec<ConfigurationChange>> {
    let base = owner.paths();
    if load(&base)?.is_none_or(|current| current.id != owner.id) {
        return Err(conflict(
            "On-demand routing ownership changed before activation.",
        ));
    }
    restore_legacy_route(&base, endpoint)?;
    configure(&mode_paths(&base), endpoint, &[Client::Codex], options)
}

/// Used on clean shutdown and by the independently running parent-exit guard.
/// The ID prevents a late old guard from restoring or retiring a newer lease.
pub fn restore(owner: &Owner) -> Result<Vec<ConfigurationChange>> {
    let base = owner.paths();
    if load(&base)?.is_none_or(|current| current.id != owner.id) {
        return Ok(Vec::new());
    }
    super::recover(&base, false)?;
    let selected = mode_paths(&base);
    let resolved = filesystem::resolve(&selected, &[Client::Codex])?;
    let locks = transaction::lock(&selected, &resolved)?;
    let Some(current_owner) = load(&base)? else {
        return Ok(Vec::new());
    };
    if current_owner.id != owner.id {
        return Ok(Vec::new());
    }
    let mut changes = transaction::resume_before_write(&selected, &resolved, false)?;
    let manifest_path = resolved.backup.join(MANIFEST);
    let manifest_raw = filesystem::read(&manifest_path)?;
    let mut manifest = documents::manifest(manifest_raw.as_deref())?;
    let had_mode = manifest.entries.contains_key(&Client::Codex);
    if let Some(entry) = manifest.entries.get(&Client::Codex) {
        let target = &resolved.targets[&Client::Codex];
        let current = filesystem::read(target)?;
        let (saved, backup) = original(
            Client::Codex,
            target,
            current.as_deref(),
            entry,
            &resolved.backup,
            true,
        )?;
        let after = if current.as_deref().map(filesystem::hash).as_deref()
            == Some(entry.post_sha256.as_str())
        {
            saved.clone()
        } else {
            documents::restore_route_fields(
                current.as_deref(),
                saved.as_deref(),
                Some(&owner.applied),
            )?
        };
        let mut reads = BTreeMap::from([
            (target.clone(), current.clone()),
            (manifest_path.clone(), manifest_raw.clone()),
        ]);
        let mut edits = vec![Edit::new(
            EditKind::Target(Client::Codex),
            target.clone(),
            current,
            after,
        )];
        manifest.entries.remove(&Client::Codex);
        let manifest_after = if manifest.entries.is_empty() {
            None
        } else {
            Some(documents::serialize(&manifest)?)
        };
        edits.push(Edit::new(
            EditKind::Manifest,
            manifest_path,
            manifest_raw,
            manifest_after,
        ));
        if let Some(path) = &backup {
            reads.insert(path.clone(), saved.clone());
            edits.push(Edit::new(
                EditKind::Original(Client::Codex),
                path.clone(),
                saved,
                None,
            ));
        }
        transaction::commit(&resolved, Operation::RestoreFields, edits, reads)?;
        changes.push(ConfigurationChange {
            client: Client::Codex,
            path: target.clone(),
            changed: true,
            backup: None,
            action: "restore normal routing; preserve user edits".into(),
        });
    }
    drop(locks);
    // A crash before the on-demand transaction began can still leave the old
    // persistent adapter route. Recover it using the separately verified original.
    if !had_mode && let Some(Some(endpoint)) = owner.applied.get("openai_base_url") {
        changes.extend(restore_legacy_fields(
            &base,
            endpoint,
            Some(&owner.applied),
        )?);
    }
    let _locks = transaction::lock(&selected, &resolved)?;
    if load(&base)?.is_none_or(|current| current.id != owner.id) {
        return Ok(changes);
    }
    let owner_path = resolved.backup.join(OWNER);
    if let Some(raw) = filesystem::read(&owner_path)? {
        filesystem::remove_known(&owner_path, &raw)?;
    }
    Ok(changes)
}

pub fn load_directory(directory: &Path) -> Result<Option<Owner>> {
    let directory = filesystem::absolute(directory, true)?;
    if directory.file_name().and_then(|name| name.to_str()) != Some("on-demand") {
        return Err(invalid("Unexpected on-demand recovery directory."));
    }
    let Some(raw) = filesystem::read(&directory.join(OWNER))? else {
        return Ok(None);
    };
    let owner: Owner = serde_json::from_value(documents::json(&raw)?)
        .map_err(|_| invalid("Invalid on-demand recovery receipt."))?;
    if filesystem::key(&mode_paths(&owner.paths()).backup_dir)? != filesystem::key(&directory)? {
        return Err(conflict(
            "The recovery receipt belongs to a different directory.",
        ));
    }
    load(&owner.paths())
}

/// Only identify a legacy local route when the original backup still belongs
/// to this file. Non-default ports also require the unchanged old post-image.
pub fn legacy_local_route(
    base: &ClientPaths,
    requested_port: u16,
) -> Result<Option<(String, std::net::SocketAddr)>> {
    let resolved = filesystem::resolve(base, &[Client::Codex])?;
    let _locks = transaction::lock(base, &resolved)?;
    transaction::resume_before_write(base, &resolved, false)?;
    let raw = filesystem::read(&resolved.targets[&Client::Codex])?;
    let settings = documents::codex_settings(raw.as_deref())?;
    if settings.provider != "openai" {
        return Ok(None);
    }
    let Some(endpoint) = settings.endpoint else {
        return Ok(None);
    };
    let url =
        url::Url::parse(&endpoint).map_err(|_| invalid("Invalid existing Codex endpoint."))?;
    if url.scheme() != "http"
        || !matches!(url.path().trim_end_matches('/'), "" | "/v1")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Ok(None);
    }
    let address = match url.host_str() {
        Some("127.0.0.1" | "localhost") => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        Some("[::1]" | "::1") => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        _ => return Ok(None),
    };
    let Some(port) = url.port() else {
        return Ok(None);
    };
    let manifest =
        documents::manifest(filesystem::read(&resolved.backup.join(MANIFEST))?.as_deref())?;
    let Some(entry) = manifest.entries.get(&Client::Codex) else {
        if port == 5001 || port == requested_port {
            return Err(conflict(
                "The existing local adapter route has no verified normal backup; it was not guessed or overwritten.",
            ));
        }
        return Ok(None);
    };
    if port != 5001
        && port != requested_port
        && raw.as_deref().map(filesystem::hash).as_deref() != Some(entry.post_sha256.as_str())
    {
        return Ok(None);
    }
    original(
        Client::Codex,
        &resolved.targets[&Client::Codex],
        raw.as_deref(),
        entry,
        &resolved.backup,
        true,
    )?;
    Ok(Some((endpoint, std::net::SocketAddr::new(address, port))))
}
