//! Owned optional image registration; reuses Codex target locks and its journal.
use super::*;
use adapter_runtime::images::ImageConfig;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use toml_edit::{DocumentMut, Item, Table};

#[cfg(not(test))]
use crate::windows_credentials as credentials;

#[cfg(test)]
pub(super) mod credentials {
    use super::*;
    use std::cell::RefCell;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(in crate::config) enum Operation {
        Read,
        Write,
        Delete,
    }
    #[derive(Default)]
    struct Store {
        values: BTreeMap<(String, String), String>,
        failure: Option<Operation>,
        calls: Vec<(Operation, String, String)>,
    }
    thread_local! { static STORE: RefCell<Store> = RefCell::default(); }

    pub(in crate::config) fn fail_next(operation: Operation) {
        STORE.with(|store| assert!(store.borrow_mut().failure.replace(operation).is_none()));
    }
    pub(in crate::config) fn take_calls() -> Vec<(Operation, String, String)> {
        STORE.with(|store| std::mem::take(&mut store.borrow_mut().calls))
    }
    fn record(operation: Operation, service: &str, key: &str) -> Result<()> {
        STORE.with(|store| {
            let mut store = store.borrow_mut();
            // Record target identities, never credential values.
            store.calls.push((operation, service.into(), key.into()));
            if store.failure == Some(operation) {
                store.failure = None;
                return Err(AdapterError::new(
                    500,
                    "credential_test_failure",
                    "Injected credential operation failure.",
                ));
            }
            Ok(())
        })
    }
    pub(in crate::config) fn read(service: &str, key: &str) -> Result<Option<String>> {
        record(Operation::Read, service, key)?;
        Ok(STORE.with(|store| {
            store
                .borrow()
                .values
                .get(&(service.into(), key.into()))
                .cloned()
        }))
    }
    pub(in crate::config) fn write(service: &str, key: &str, value: &str) -> Result<()> {
        record(Operation::Write, service, key)?;
        STORE.with(|store| {
            store
                .borrow_mut()
                .values
                .insert((service.into(), key.into()), value.into());
        });
        Ok(())
    }
    pub(in crate::config) fn delete(service: &str, key: &str) -> Result<bool> {
        record(Operation::Delete, service, key)?;
        Ok(STORE.with(|store| {
            store
                .borrow_mut()
                .values
                .remove(&(service.into(), key.into()))
                .is_some()
        }))
    }
}

const STATE: &str = "image-provider.json";
const SERVER: &str = "github_adapter_image";
const SERVICE: &str = "GitHubAdapter.ImageProvider";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    version: u32,
    enabled: bool,
    pending: bool,
    provider: ImageConfig,
    key_id: String,
    previous_key: Option<String>,
    codex: PathBuf,
    backup: PathBuf,
    program: PathBuf,
    previous_program: Option<PathBuf>,
    created_config: bool,
    created_parent: bool,
    skill_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_skill_hash: Option<String>,
}
impl Registration {
    fn service(&self) -> String {
        format!(
            "{SERVICE}.{}",
            filesystem::hash(self.codex.to_string_lossy().to_lowercase().as_bytes())
        )
    }
}
fn valid_key_id(key: &str) -> bool {
    key.len() == 81
        && key.as_bytes()[16] == b'-'
        && key
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 16 || b.is_ascii_hexdigit())
}
fn unique() -> String {
    filesystem::hash(
        format!(
            "{}:{:?}:{}",
            std::process::id(),
            std::time::SystemTime::now(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )
        .as_bytes(),
    )
}
fn config_hash(config: &ImageConfig) -> Result<String> {
    Ok(filesystem::hash(
        &serde_json::to_vec(config)
            .map_err(|_| invalid("Could not serialize image configuration."))?,
    )[..16]
        .into())
}
fn read(resolved: &filesystem::Resolved) -> Result<Option<Registration>> {
    let Some(raw) = filesystem::read(&resolved.backup.join(STATE))? else {
        return Ok(None);
    };
    let state: Registration =
        serde_json::from_slice(&raw).map_err(|_| invalid("Invalid owned image registration."))?;
    // Receipt ownership must remain readable for cleanup even if a newer
    // network policy rejects an older provider. Validate before enable/use below.
    if state.version != 1
        || state.codex != resolved.targets[&Client::Codex]
        || state.backup != resolved.backup
        || !state.program.is_absolute()
        || state
            .previous_program
            .as_ref()
            .is_some_and(|p| !p.is_absolute())
        || !valid_key_id(&state.key_id)
        || state
            .previous_key
            .as_deref()
            .is_some_and(|key| !valid_key_id(key))
        || !state
            .key_id
            .starts_with(&format!("{}-", config_hash(&state.provider)?))
        || !state.key_id[17..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid(
            "Image registration does not match its selected paths or credential scope.",
        ));
    }
    Ok(Some(state))
}
fn save(resolved: &filesystem::Resolved, state: &Registration) -> Result<()> {
    // The private receipt must remain independently writable so disable can
    // persist revocation intent even when the configuration/skill journal conflicts.
    let path = resolved.backup.join(STATE);
    let before = filesystem::read(&path)?;
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|_| invalid("Could not serialize image registration."))?;
    if before.as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    let id = unique();
    let staged = resolved.backup.join(format!(".image-{id}.tmp"));
    let displaced = resolved.backup.join(format!(".image-{id}.old"));
    filesystem::write_private(&staged, &bytes)?;
    if let Err(error) = filesystem::assert_current(&path, before.as_deref()) {
        filesystem::remove_known(&staged, &bytes)?;
        return Err(error);
    }
    if let Some(before) = before {
        filesystem::replace(&path, &staged, &displaced)?;
        filesystem::remove_known(&displaced, &before)?;
    } else {
        filesystem::move_new(&staged, &path)?;
    }
    Ok(())
}

fn skill_paths(resolved: &filesystem::Resolved) -> ClientPaths {
    ClientPaths {
        codex_config: Some(resolved.targets[&Client::Codex].clone()),
        claude_settings: None,
        // An optional skill conflict must not block ordinary Codex route restoration.
        backup_dir: resolved.backup.join("image-skill"),
    }
}

fn lock(
    paths: &ClientPaths,
    resolved: &filesystem::Resolved,
) -> Result<(filesystem::MutexGuard, transaction::Locks)> {
    // Acquire both backup locks before the shared Codex target lock. The same
    // transaction engine can also recover the skill directory explicitly.
    let skill = skill_paths(resolved);
    // Resolve only the lock identity here. Validate skill storage after revocation,
    // so malformed optional storage cannot prevent a known key deletion attempt.
    let skill_lock = filesystem::MutexGuard::acquire(&format!(
        "backup:{}",
        filesystem::key(&skill.backup_dir)?
    ))?;
    Ok((skill_lock, transaction::lock(paths, resolved)?))
}

fn resume_skill(resolved: &filesystem::Resolved) -> Result<()> {
    let paths = skill_paths(resolved);
    let resolved = filesystem::resolve(&paths, &[Client::Codex])?;
    transaction::resume_before_write(&paths, &resolved, false).map(|_| ())
}

fn publish_skill(
    resolved: &filesystem::Resolved,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
) -> Result<()> {
    let selected = skill_paths(resolved);
    let resolved = filesystem::resolve(&selected, &[Client::Codex])?;
    let path = skill_path(&resolved.targets[&Client::Codex]);
    if before == after {
        return filesystem::assert_current(&path, before.as_deref());
    }
    transaction::commit(
        &resolved,
        Operation::Patch,
        vec![Edit::new(
            EditKind::ImageSkill,
            path.clone(),
            before.clone(),
            after,
        )],
        BTreeMap::from([(path, before)]),
    )
}

fn document(raw: Option<&[u8]>) -> Result<DocumentMut> {
    std::str::from_utf8(raw.unwrap_or(b""))
        .map_err(|_| invalid("Codex configuration must be UTF-8."))?
        .parse()
        .map_err(|_| invalid("Codex configuration must be valid TOML."))
}
fn block(program: &Path, state: &Registration) -> Result<Item> {
    let mut table = Table::new();
    table["command"] = toml_edit::value(filesystem::path_text(program)?);
    let mut args = toml_edit::Array::new();
    for arg in [
        "image-mcp".to_owned(),
        "--codex-config".into(),
        filesystem::path_text(&state.codex)?,
        "--backup-dir".into(),
        filesystem::path_text(&state.backup)?,
    ] {
        args.push(arg);
    }
    table["args"] = toml_edit::value(args);
    table["startup_timeout_sec"] = toml_edit::value(10);
    table["tool_timeout_sec"] = toml_edit::value(300);
    Ok(Item::Table(table))
}
fn registered(doc: &DocumentMut) -> Option<&Item> {
    doc.get("mcp_servers").and_then(|v| v.get(SERVER))
}
fn owned(current: &Item, state: &Registration) -> Result<bool> {
    let matches = |program: &Path| -> Result<bool> {
        Ok(current.to_string() == block(program, state)?.to_string())
    };
    Ok(matches(&state.program)?
        || state
            .previous_program
            .as_deref()
            .map(matches)
            .transpose()?
            .unwrap_or(false))
}
fn owns_skill(state: &Registration, bytes: &[u8]) -> bool {
    let hash = filesystem::hash(bytes);
    state.skill_hash.as_deref() == Some(&hash) || state.pending_skill_hash.as_deref() == Some(&hash)
}
pub(super) fn skill_path(codex: &Path) -> PathBuf {
    codex
        .parent()
        .unwrap()
        .join("skills")
        .join("github-adapter-image")
        .join("SKILL.md")
}
fn quote(path: &Path) -> String {
    let replacement = if cfg!(windows) { "''" } else { "'\\''" };
    format!("'{}'", path.to_string_lossy().replace('\'', replacement))
}
fn skill(state: &Registration) -> String {
    let document = format!(
        "---\nname: github-adapter-image\ndescription: Use the explicitly configured optional image provider through its MCP tool or native helper.\n---\n\nGenerate images only when requested by the user. Prefer the github_adapter_image MCP generate_image tool.\nIf that tool is missing from an existing task's cached catalog, BEFORE submitting any generation, use the native helper below.\nDo not install Python/SDKs, change the coding provider, request another API key, or select a different image provider.\nAfter a generation has been dispatched, never automatically repeat it or switch transports after an error or ambiguous result. Report the error and wait for the user.\n\nWrite the requested prompt to a task-local UTF-8 file, then run exactly once (replace the two output placeholders):\n\n```powershell\n& {} image-generate --codex-config {} --backup-dir {} --prompt-file '<absolute prompt file>' --output '<absolute image file>'\n```\n\nFor non-generating readiness checks use the same executable and profile arguments with image-status --check. An existing client may require one restart to discover this skill.\n",
        quote(&state.program),
        quote(&state.codex),
        quote(&state.backup)
    );
    if cfg!(windows) {
        document
    } else {
        document.replace("```powershell\n& ", "```sh\n")
    }
}
fn patch(resolved: &filesystem::Resolved, state: &Registration, enabling: bool) -> Result<()> {
    let target = &state.codex;
    let before = filesystem::read(target)?;
    let mut doc = document(before.as_deref())?;
    if let Some(existing) = registered(&doc)
        && !owned(existing, state)?
    {
        if !enabling {
            return Ok(());
        }
        return Err(conflict(
            "The image MCP entry was edited or belongs to another tool; it was not overwritten.",
        ));
    }
    if enabling {
        if doc.get("mcp_servers").is_none() {
            doc["mcp_servers"] = Item::Table(Table::new());
        }
        let table = doc["mcp_servers"]
            .as_table_mut()
            .ok_or_else(|| conflict("mcp_servers must be a TOML table."))?;
        table[SERVER] = block(&state.program, state)?;
    } else if let Some(table) = doc.get_mut("mcp_servers").and_then(Item::as_table_mut) {
        table.remove(SERVER);
        if table.is_empty() && state.created_parent {
            doc.remove("mcp_servers");
        }
    }
    let after = if !enabling
        && state.created_config
        && doc.as_table().is_empty()
        && doc.to_string().trim().is_empty()
    {
        None
    } else {
        Some(doc.to_string().into_bytes())
    };
    if before != after {
        transaction::commit(
            resolved,
            Operation::Patch,
            vec![Edit::new(
                EditKind::Target(Client::Codex),
                target.clone(),
                before.clone(),
                after,
            )],
            BTreeMap::from([(target.clone(), before)]),
        )?;
    }
    Ok(())
}
fn finish_enable(resolved: &filesystem::Resolved, state: &mut Registration) -> Result<()> {
    if credentials::read(&state.service(), &state.key_id)?.is_none_or(|key| key.is_empty()) {
        return Err(invalid(
            "Image credential setup was interrupted. Disable the incomplete registration and enable it again with the key; no generation was attempted.",
        ));
    }
    patch(resolved, state, true)?;
    checkpoint("image:config")?;
    let path = skill_path(&state.codex);
    let current = filesystem::read(&path)?;
    let content = skill(state).into_bytes();
    if current.is_none()
        || current
            .as_deref()
            .is_some_and(|bytes| owns_skill(state, bytes))
        || current.as_deref() == Some(content.as_slice())
    {
        // The exact authorized bytes remain the last-owned version until publication.
        state.skill_hash = current.as_deref().map(filesystem::hash);
        state.pending_skill_hash = Some(filesystem::hash(&content));
        save(resolved, state)?;
        checkpoint("image:skill-authorized")?;
        publish_skill(resolved, current, Some(content))?;
        state.skill_hash = state.pending_skill_hash.take();
    }
    checkpoint("image:skill")?;
    if let Some(key) = &state.previous_key {
        credentials::delete(&state.service(), key)?;
    }
    state.previous_key = None;
    state.pending_skill_hash = None;
    state.pending = false;
    state.previous_program = None;
    save(resolved, state)
}

fn resume_enable(
    resolved: &filesystem::Resolved,
    state: &mut Registration,
    program: PathBuf,
) -> Result<()> {
    if !program.is_file() {
        return Err(invalid("The native image helper executable is missing."));
    }
    if state.pending && state.program != program {
        // Finish A -> B while A is still recognized before preparing B -> C.
        finish_enable(resolved, state)?;
    }
    if state.program != program {
        state.previous_program = Some(state.program.clone());
        state.program = program;
    }
    // Persist the pending receipt before updating the owned MCP entry or skill.
    state.pending = true;
    save(resolved, state)?;
    finish_enable(resolved, state)
}

pub fn enable(
    paths: &ClientPaths,
    program: &Path,
    provider: ImageConfig,
    key: &str,
) -> Result<Value> {
    provider.validate()?;
    if key.is_empty() || key.chars().any(char::is_control) {
        return Err(invalid(
            "The image API key must be nonempty and contain no control characters.",
        ));
    }
    let resolved = filesystem::resolve(paths, &[Client::Codex])?;
    let _locks = lock(paths, &resolved)?;
    transaction::resume_before_write(paths, &resolved, false)?;
    resume_skill(&resolved)?;
    let program = filesystem::absolute(program, false)?;
    if !program.is_file() {
        return Err(invalid(
            "The native image helper executable does not exist.",
        ));
    }
    let old = read(&resolved)?;
    if old.as_ref().is_some_and(|s| s.pending) {
        return Err(conflict(
            "Finish or disable the pending image registration before changing providers.",
        ));
    }
    let current = filesystem::read(&resolved.targets[&Client::Codex])?;
    let doc = document(current.as_deref())?;
    if let Some(existing) = registered(&doc)
        && !old
            .as_ref()
            .map(|state| owned(existing, state))
            .transpose()?
            .unwrap_or(false)
    {
        return Err(conflict(
            "The image MCP name is already owned by another configuration.",
        ));
    }
    let key_id = format!("{}-{}", config_hash(&provider)?, unique());
    let mut state = Registration {
        version: 1,
        enabled: true,
        pending: true,
        provider,
        key_id,
        previous_key: old.as_ref().map(|s| s.key_id.clone()),
        codex: resolved.targets[&Client::Codex].clone(),
        backup: resolved.backup.clone(),
        program,
        previous_program: old.as_ref().map(|s| s.program.clone()),
        created_config: old.as_ref().map_or(current.is_none(), |s| s.created_config),
        created_parent: old
            .as_ref()
            .map_or(doc.get("mcp_servers").is_none(), |s| s.created_parent),
        skill_hash: old.as_ref().and_then(|s| s.skill_hash.clone()),
        pending_skill_hash: None,
    };
    // A new credential ID for every activation prevents old/new endpoint or key
    // pairs from being mixed, even across a crash during a provider change.
    // Persist the receipt before touching the external credential store, so an
    // interrupted activation never leaves an untracked usable credential.
    save(&resolved, &state)?;
    if let Err(error) = credentials::write(&state.service(), &state.key_id, key) {
        credentials::delete(&state.service(), &state.key_id)?;
        if let Some(old) = &old {
            save(&resolved, old)?;
        } else if let Some(raw) = filesystem::read(&resolved.backup.join(STATE))? {
            filesystem::remove_known(&resolved.backup.join(STATE), &raw)?;
        }
        return Err(error);
    }
    checkpoint("image:state")?;
    finish_enable(&resolved, &mut state)?;
    drop(_locks);
    status(paths)
}

pub fn repair(paths: &ClientPaths, program: &Path) -> Result<Value> {
    let resolved = filesystem::resolve(paths, &[Client::Codex])?;
    let locks = lock(paths, &resolved)?;
    transaction::resume_before_write(paths, &resolved, false)?;
    resume_skill(&resolved)?;
    let mut state = read(&resolved)?.ok_or_else(|| {
        invalid("No image provider is configured. Use image-enable with --endpoint and --model.")
    })?;
    if !state.enabled {
        return Err(invalid(
            "Finish disabling the previous image registration before enabling a provider.",
        ));
    }
    state.provider.validate()?;
    let program = filesystem::absolute(program, false)?;
    resume_enable(&resolved, &mut state, program)?;
    drop(locks);
    status(paths)
}

pub fn disable(paths: &ClientPaths) -> Result<Value> {
    let resolved = filesystem::resolve(paths, &[Client::Codex])?;
    let locks = lock(paths, &resolved)?;
    let mut registration = read(&resolved)?;
    if let Some(state) = registration.as_mut() {
        if state.pending && state.pending_skill_hash.is_none() {
            // Read old pending receipts without treating an ordinary user-created skill as owned.
            state.pending_skill_hash = Some(filesystem::hash(skill(state).as_bytes()));
        }
        state.enabled = false;
        state.pending = true;
        save(&resolved, state)?;
        checkpoint("image:disable")?;
        // Revocation must not depend on user-editable TOML or skill cleanup.
        // Attempt both known key slots even when the first deletion fails.
        let current_key = credentials::delete(&state.service(), &state.key_id);
        let previous_key = state
            .previous_key
            .as_ref()
            .map(|key| credentials::delete(&state.service(), key))
            .transpose();
        current_key?;
        previous_key?;
    }
    transaction::resume_before_write(paths, &resolved, false)?;
    resume_skill(&resolved)?;
    if let Some(state) = registration {
        patch(&resolved, &state, false)?;
        let path = skill_path(&state.codex);
        if let Some(raw) = filesystem::read(&path)?
            && owns_skill(&state, &raw)
        {
            publish_skill(&resolved, Some(raw), None)?;
        }
        let path = resolved.backup.join(STATE);
        if let Some(raw) = filesystem::read(&path)? {
            filesystem::remove_known(&path, &raw)?;
        }
    }
    drop(locks);
    status(paths)
}

pub fn startup(paths: &ClientPaths, program: &Path) -> Result<()> {
    let resolved = filesystem::resolve(paths, &[Client::Codex])?;
    if filesystem::read(&resolved.backup.join(STATE))?.is_none() {
        return Ok(());
    }
    let locks = lock(paths, &resolved)?;
    let Some(mut state) = read(&resolved)? else {
        return Ok(());
    };
    if !state.enabled {
        let pending = state.pending;
        drop(locks);
        if pending {
            disable(paths)?;
        }
        return Ok(());
    }
    transaction::resume_before_write(paths, &resolved, false)?;
    resume_skill(&resolved)?;
    let program = filesystem::absolute(program, false)?;
    state.provider.validate()?;
    if !state.pending && state.program == program {
        return Ok(());
    }
    let doc = document(filesystem::read(&state.codex)?.as_deref())?;
    if !state.pending && registered(&doc).is_none() {
        return Err(conflict(
            "The image MCP entry was removed by the user; use explicit image-enable to restore it.",
        ));
    }
    resume_enable(&resolved, &mut state, program)
}

pub fn status(paths: &ClientPaths) -> Result<Value> {
    let resolved = filesystem::resolve(paths, &[Client::Codex])?;
    let _locks = lock(paths, &resolved)?;
    transaction::require_no_journal(&resolved.backup)?;
    transaction::require_no_journal(&skill_paths(&resolved).backup_dir)?;
    let Some(state) = read(&resolved)? else {
        return Ok(
            json!({"configured":false,"enabled":false,"mcp_registered":false,"provider_inference_verified":false}),
        );
    };
    let doc = document(filesystem::read(&state.codex)?.as_deref())?;
    let registered = registered(&doc)
        .map(|v| owned(v, &state))
        .transpose()?
        .unwrap_or(false);
    let credential = credentials::read(&state.service(), &state.key_id)?.is_some();
    Ok(
        json!({"configured":true,"enabled":state.enabled,"pending":state.pending,"mcp_registered":registered,
        "credential_available":credential,"provider_valid":state.provider.validate().is_ok(),"endpoint":state.provider.endpoint,"model":state.provider.model,"protocol":state.provider.protocol,"supported_sizes":state.provider.supported_sizes(),
        "helper":state.program,"provider_inference_verified":false,
        "skill_managed":filesystem::read(&skill_path(&state.codex))?.as_deref().is_some_and(|bytes| owns_skill(&state, bytes))}),
    )
}

pub fn snapshot(paths: &ClientPaths) -> Result<(ImageConfig, String)> {
    let resolved = filesystem::resolve(paths, &[Client::Codex])?;
    let _locks = lock(paths, &resolved)?;
    transaction::require_no_journal(&resolved.backup)?;
    transaction::require_no_journal(&skill_paths(&resolved).backup_dir)?;
    let state=read(&resolved)?.filter(|s|s.enabled && !s.pending).ok_or_else(||invalid("Image generation is disabled or registration is incomplete. Use image-enable explicitly; do not switch providers or repeat generation."))?;
    state.provider.validate()?;
    let key = credentials::read(&state.service(), &state.key_id)?
        .filter(|k| !k.is_empty())
        .ok_or_else(|| invalid("The configured image credential is unavailable."))?;
    Ok((state.provider, key))
}

pub struct GenerationLock {
    _guard: filesystem::MutexGuard,
}
pub fn generation_lock(paths: &ClientPaths) -> Result<GenerationLock> {
    let resolved = filesystem::resolve(paths, &[Client::Codex])?;
    Ok(GenerationLock {
        _guard: filesystem::MutexGuard::acquire(&format!(
            "image-generation:{}",
            filesystem::key(&resolved.targets[&Client::Codex])?
        ))
        .map_err(|_| {
            AdapterError::new(
                429,
                "image_busy",
                "Another image request is active. No generation was submitted.",
            )
        })?,
    })
}
pub struct ImageOutput {
    file: std::fs::File,
    path: PathBuf,
}
pub fn reserve_output(path: &Path) -> Result<ImageOutput> {
    let path = filesystem::absolute(path, false)?;
    filesystem::parents(&path)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| {
            conflict("The image output exists or cannot be created; no generation was submitted.")
        })?;
    Ok(ImageOutput { file, path })
}
impl ImageOutput {
    pub fn write(mut self, bytes: &[u8]) -> Result<PathBuf> {
        self.file.write_all(bytes).and_then(|_|self.file.sync_all())
            .map_err(|_|invalid("The image output could not be completely written. Do not repeat generation automatically."))?;
        Ok(self.path)
    }
}
