use super::*;
use filesystem::{MutexGuard, Resolved};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "client",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum EditKind {
    Target(Client),
    Original(Client),
    Manifest,
    ImageSkill,
}

impl EditKind {
    fn checkpoint(self, boundary: &str, index: usize) -> Result<()> {
        checkpoint(&if self == Self::ImageSkill {
            format!("image-skill:{boundary}")
        } else {
            format!("{boundary}:{index}")
        })
    }

    fn path(self, resolved: &Resolved) -> Result<PathBuf> {
        match self {
            Self::Target(client) => resolved
                .targets
                .get(&client)
                .cloned()
                .ok_or_else(|| invalid("Recovery requires the original selected client paths.")),
            Self::Original(client) => {
                if !resolved.targets.contains_key(&client) {
                    return Err(invalid(
                        "Recovery references an unselected original backup.",
                    ));
                }
                Ok(resolved.backup.join(client.original()))
            }
            Self::Manifest => Ok(resolved.backup.join(MANIFEST)),
            Self::ImageSkill => resolved
                .targets
                .get(&Client::Codex)
                .map(|path| images::skill_path(path))
                .ok_or_else(|| invalid("Image skill recovery requires the selected Codex path.")),
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Operation {
    Configure,
    Restore,
    RestoreFields,
    Patch,
}

pub(super) struct Edit {
    kind: EditKind,
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

impl Edit {
    pub(super) fn new(
        kind: EditKind,
        path: PathBuf,
        before: Option<Vec<u8>>,
        after: Option<Vec<u8>>,
    ) -> Self {
        Self {
            kind,
            path,
            before,
            after,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedEdit {
    role: EditKind,
    before_sha256: Option<String>,
    after_sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedRead {
    role: EditKind,
    sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    id: String,
    operation: Operation,
    backup_dir: String,
    targets: BTreeMap<Client, String>,
    edits: Vec<RecordedEdit>,
    checks: Vec<RecordedRead>,
}

impl Journal {
    fn directory(&self, backup: &Path) -> PathBuf {
        backup.join(format!(".native-configuration-{}", self.id))
    }

    fn copy(&self, backup: &Path, index: usize, side: &str) -> PathBuf {
        self.directory(backup).join(format!("{index:02}.{side}"))
    }

    fn done(&self, backup: &Path, index: usize) -> PathBuf {
        self.directory(backup).join(format!("{index:02}.done"))
    }

    fn staged(&self, target: &Path, index: usize, suffix: &str) -> Result<PathBuf> {
        let parent = target
            .parent()
            .ok_or_else(|| invalid("A journal target has no parent."))?;
        Ok(parent.join(format!(".github-adapter-{}-{index:02}.{suffix}", self.id)))
    }
}

pub(super) struct Locks {
    _targets: Vec<MutexGuard>,
    _backup: MutexGuard,
}

pub(super) fn lock(paths: &ClientPaths, resolved: &Resolved) -> Result<Locks> {
    let backup = MutexGuard::acquire(&format!("backup:{}", filesystem::key(&resolved.backup)?))?;
    if filesystem::read(&resolved.backup.join(".configuration.lock"))?.is_some() {
        return Err(conflict(
            "A legacy Python configuration lock exists; no native changes or lock deletion were performed.",
        ));
    }
    let mut keys: BTreeSet<String> = resolved
        .targets
        .values()
        .map(|path| filesystem::key(path).map(|key| format!("target:{key}")))
        .collect::<Result<_>>()?;
    if let Some((_, journal)) = load(&resolved.backup)? {
        let pending = journal_paths(paths, &resolved.backup, &journal)?;
        for path in pending.targets.values() {
            keys.insert(format!("target:{}", filesystem::key(path)?));
        }
    }
    let targets = keys
        .into_iter()
        .map(|key| MutexGuard::acquire(&key))
        .collect::<Result<Vec<_>>>()?;
    Ok(Locks {
        _targets: targets,
        _backup: backup,
    })
}

pub(super) fn require_no_journal(backup: &Path) -> Result<()> {
    if filesystem::read(&backup.join(JOURNAL))?.is_some() {
        return Err(conflict(
            "An interrupted native configuration transaction requires recover before inspection or preview.",
        ));
    }
    Ok(())
}

pub(super) fn resume_before_write(
    paths: &ClientPaths,
    resolved: &Resolved,
    dry_run: bool,
) -> Result<Vec<ConfigurationChange>> {
    if dry_run {
        require_no_journal(&resolved.backup)?;
        return Ok(Vec::new());
    }
    recover_locked(paths, resolved, false)
}

fn load(backup: &Path) -> Result<Option<(Vec<u8>, Journal)>> {
    let Some(raw) = filesystem::read(&backup.join(JOURNAL))? else {
        return Ok(None);
    };
    let value = documents::json(&raw)?;
    documents::exact_keys(
        &value,
        &[
            "version",
            "id",
            "operation",
            "backup_dir",
            "targets",
            "edits",
            "checks",
        ],
    )?;
    let edits = value["edits"]
        .as_array()
        .ok_or_else(|| invalid("Invalid native journal edits."))?;
    let checks = value["checks"]
        .as_array()
        .ok_or_else(|| invalid("Invalid native journal checks."))?;
    for edit in edits {
        documents::exact_keys(edit, &["role", "before_sha256", "after_sha256"])?;
    }
    for check in checks {
        documents::exact_keys(check, &["role", "sha256"])?;
    }
    let journal: Journal = serde_json::from_value(value)
        .map_err(|_| invalid("The native journal contains invalid or unsupported fields."))?;
    validate_journal(&journal)?;
    Ok(Some((raw, journal)))
}

fn validate_journal(journal: &Journal) -> Result<()> {
    if journal.version != 1
        || !documents::valid_hash(&journal.id)
        || journal.targets.is_empty()
        || journal.targets.len() > 2
        || journal.edits.is_empty()
        || journal.edits.len() > 5
        || journal.checks.len() > 5
        || !Path::new(&journal.backup_dir).is_absolute()
    {
        return Err(invalid(
            "The native configuration journal has an unsupported shape or version.",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut rank = 0;
    let mut has_target = false;
    let valid_role = |role: EditKind| match role {
        EditKind::Target(client) | EditKind::Original(client) => {
            journal.targets.contains_key(&client)
        }
        EditKind::Manifest => true,
        EditKind::ImageSkill => journal.targets.contains_key(&Client::Codex),
    };
    for edit in &journal.edits {
        if !valid_role(edit.role)
            || !seen.insert(edit.role)
            || edit
                .before_sha256
                .as_deref()
                .is_some_and(|value| !documents::valid_hash(value))
            || edit
                .after_sha256
                .as_deref()
                .is_some_and(|value| !documents::valid_hash(value))
            || (edit.before_sha256 == edit.after_sha256
                && !matches!(
                    (journal.operation, edit.role),
                    (
                        Operation::Restore | Operation::RestoreFields,
                        EditKind::Target(_)
                    )
                ))
        {
            return Err(invalid(
                "The native journal contains invalid, duplicate or unchanged edits.",
            ));
        }
        let next_rank = match (journal.operation, edit.role) {
            (Operation::Configure, EditKind::Original(_)) => {
                if edit.before_sha256.is_some() || edit.after_sha256.is_none() {
                    return Err(invalid(
                        "A configure transaction must never replace an original backup.",
                    ));
                }
                0
            }
            (Operation::Configure, EditKind::Target(_))
            | (Operation::Restore | Operation::RestoreFields, EditKind::Manifest) => 1,
            (Operation::Configure, EditKind::Manifest) => {
                if edit.after_sha256.is_none() {
                    return Err(invalid("A configure transaction requires its v1 manifest."));
                }
                2
            }
            (
                Operation::Restore | Operation::RestoreFields | Operation::Patch,
                EditKind::Target(_),
            ) => 0,
            (Operation::Restore | Operation::RestoreFields, EditKind::Original(_)) => {
                if edit.before_sha256.is_none() || edit.after_sha256.is_some() {
                    return Err(invalid(
                        "A restore transaction may only remove its verified original backup.",
                    ));
                }
                2
            }
            (Operation::Patch, EditKind::ImageSkill) => 0,
            (_, EditKind::ImageSkill) => {
                return Err(invalid(
                    "An image skill can only be edited by an owned patch.",
                ));
            }
            (Operation::Patch, _) => {
                return Err(invalid(
                    "An owned patch may only edit selected settings or the image skill.",
                ));
            }
        };
        if next_rank < rank {
            return Err(invalid(
                "The native journal has an invalid operation ordering.",
            ));
        }
        rank = next_rank;
        has_target |= matches!(edit.role, EditKind::Target(_) | EditKind::ImageSkill);
    }
    if !has_target
        || (!matches!(journal.operation, Operation::Patch) && !seen.contains(&EditKind::Manifest))
    {
        return Err(invalid(
            "The native journal is missing a client or manifest operation.",
        ));
    }
    for check in &journal.checks {
        if !valid_role(check.role)
            || !seen.insert(check.role)
            || check
                .sha256
                .as_deref()
                .is_some_and(|value| !documents::valid_hash(value))
        {
            return Err(invalid(
                "The native journal contains invalid or duplicate read preconditions.",
            ));
        }
    }
    Ok(())
}

fn journal_paths(paths: &ClientPaths, backup: &Path, journal: &Journal) -> Result<Resolved> {
    let clients: Vec<_> = journal.targets.keys().copied().collect();
    let resolved = filesystem::resolve(paths, &clients)?;
    if filesystem::key(backup)?
        != filesystem::key(&filesystem::absolute(Path::new(&journal.backup_dir), true)?)?
        || filesystem::key(backup)? != filesystem::key(&resolved.backup)?
    {
        return Err(conflict(
            "The native journal belongs to a different backup directory.",
        ));
    }
    for (client, recorded) in &journal.targets {
        if !Path::new(recorded).is_absolute()
            || filesystem::key(&filesystem::absolute(Path::new(recorded), false)?)?
                != filesystem::key(&resolved.targets[client])?
        {
            return Err(conflict(
                "The native journal belongs to different selected settings paths.",
            ));
        }
    }
    Ok(resolved)
}

fn id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    filesystem::hash(
        format!(
            "{}:{}:{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )
        .as_bytes(),
    )
}

pub(super) fn commit(
    resolved: &Resolved,
    operation: Operation,
    edits: Vec<Edit>,
    reads: BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Result<()> {
    require_no_journal(&resolved.backup)?;
    for edit in &edits {
        if edit.path != edit.kind.path(resolved)? {
            return Err(invalid(
                "A transaction edit escaped its selected ownership path.",
            ));
        }
    }
    for (path, expected) in &reads {
        filesystem::assert_current(path, expected.as_deref())?;
    }
    let mut checks = Vec::new();
    for (path, before) in &reads {
        if edits.iter().any(|edit| &edit.path == path) {
            continue;
        }
        let role = if path == &resolved.backup.join(MANIFEST) {
            EditKind::Manifest
        } else if let Some((client, _)) =
            resolved.targets.iter().find(|(_, target)| *target == path)
        {
            EditKind::Target(*client)
        } else {
            let client = resolved
                .targets
                .keys()
                .find(|client| resolved.backup.join(client.original()) == *path)
                .ok_or_else(|| invalid("A transaction read escaped its selected paths."))?;
            EditKind::Original(*client)
        };
        checks.push(RecordedRead {
            role,
            sha256: before.as_deref().map(filesystem::hash),
        });
    }
    let journal = Journal {
        version: 1,
        id: id(),
        operation,
        backup_dir: filesystem::path_text(&resolved.backup)?,
        targets: resolved
            .targets
            .iter()
            .map(|(client, path)| Ok((*client, filesystem::path_text(path)?)))
            .collect::<Result<_>>()?,
        edits: edits
            .iter()
            .map(|edit| RecordedEdit {
                role: edit.kind,
                before_sha256: edit.before.as_deref().map(filesystem::hash),
                after_sha256: edit.after.as_deref().map(filesystem::hash),
            })
            .collect(),
        checks,
    };
    validate_journal(&journal)?;
    let raw = documents::serialize(&journal)?;
    for (index, edit) in edits.iter().enumerate() {
        for suffix in ["tmp", "displaced"] {
            if filesystem::read(&journal.staged(&edit.path, index, suffix)?)?.is_some() {
                return Err(conflict(
                    "A transaction staging path already exists; nothing was replaced.",
                ));
            }
        }
    }
    filesystem::private_directory(&journal.directory(&resolved.backup))?;
    for (index, edit) in edits.iter().enumerate() {
        for (side, bytes) in [
            ("before", edit.before.as_deref()),
            ("after", edit.after.as_deref()),
        ] {
            if let Some(bytes) = bytes {
                filesystem::write_private(&journal.copy(&resolved.backup, index, side), bytes)?;
                checkpoint(&format!("copy:{index}:{side}"))?;
            }
        }
    }
    let staged_journal = journal
        .directory(&resolved.backup)
        .join("unpublished-journal.json");
    filesystem::write_private(&staged_journal, &raw)?;
    checkpoint("before-journal")?;
    for (path, expected) in &reads {
        filesystem::assert_current(path, expected.as_deref())?;
    }
    filesystem::move_new(&staged_journal, &resolved.backup.join(JOURNAL))?;
    checkpoint("journal")?;
    apply(resolved, &journal, &raw, false)?;
    Ok(())
}

fn copy_bytes(
    journal: &Journal,
    backup: &Path,
    index: usize,
    side: &str,
    expected: Option<&str>,
) -> Result<Option<Vec<u8>>> {
    let path = journal.copy(backup, index, side);
    let bytes = filesystem::read(&path)?;
    if bytes.as_deref().map(filesystem::hash).as_deref() != expected {
        return Err(conflict(
            "A protected recovery image is missing or modified; recovery is blocked.",
        ));
    }
    Ok(bytes)
}

fn is_done(journal: &Journal, backup: &Path, index: usize) -> Result<bool> {
    match filesystem::read(&journal.done(backup, index))? {
        None => Ok(false),
        Some(bytes) if bytes.is_empty() => Ok(true),
        Some(_) => Err(conflict(
            "A recovery progress marker was modified; recovery is blocked.",
        )),
    }
}

struct Images {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    changed: bool,
}

fn validate_state(resolved: &Resolved, journal: &Journal) -> Result<Vec<Images>> {
    filesystem::check(&journal.directory(&resolved.backup), true)?;
    for check in &journal.checks {
        let raw = filesystem::read(&check.role.path(resolved)?)?;
        if raw.as_deref().map(filesystem::hash) != check.sha256 {
            return Err(conflict(
                "An unchanged settings/original precondition was modified; recovery is blocked.",
            ));
        }
    }
    let mut images = Vec::new();
    for (index, edit) in journal.edits.iter().enumerate() {
        let path = edit.role.path(resolved)?;
        let before = copy_bytes(
            journal,
            &resolved.backup,
            index,
            "before",
            edit.before_sha256.as_deref(),
        )?;
        let after = copy_bytes(
            journal,
            &resolved.backup,
            index,
            "after",
            edit.after_sha256.as_deref(),
        )?;
        for bytes in [before.as_deref(), after.as_deref()].into_iter().flatten() {
            match edit.role {
                EditKind::Target(client) | EditKind::Original(client) => {
                    documents::validate_document(client, Some(bytes))?
                }
                EditKind::Manifest => {
                    documents::manifest(Some(bytes))?;
                }
                EditKind::ImageSkill => {
                    std::str::from_utf8(bytes)
                        .map_err(|_| invalid("Image skill recovery bytes must be UTF-8."))?;
                }
            }
        }
        let current = filesystem::read(&path)?;
        let displaced = filesystem::read(&journal.staged(&path, index, "displaced")?)?;
        if displaced.is_some() && displaced != before {
            return Err(conflict(
                "A displaced file contains unexpected data; it was preserved and recovery is blocked.",
            ));
        }
        let staged = filesystem::read(&journal.staged(&path, index, "tmp")?)?;
        // macOS atomically swaps the target and staging files before moving the
        // captured preimage to `displaced`. Both hashes must identify that exact
        // interrupted state; unknown files remain protected by the normal checks.
        let swapped = cfg!(target_os = "macos")
            && before.is_some()
            && before != after
            && current == after
            && staged == before
            && displaced.is_none();
        if staged.as_deref().is_some_and(|staged| {
            !swapped
                && !after
                    .as_deref()
                    .is_some_and(|after| after.starts_with(staged))
        }) {
            return Err(conflict(
                "A staging file contains unknown data; it was preserved.",
            ));
        }
        let done = is_done(journal, &resolved.backup, index)?;
        let known_missing = !done
            && current.is_none()
            && before.is_some()
            && after.is_some()
            && displaced == before;
        if (done && current != after) || (current != before && current != after && !known_missing) {
            return Err(conflict(
                "A settings, backup, or manifest file has unknown content; recovery will not overwrite it.",
            ));
        }
        images.push(Images {
            path,
            before,
            changed: current != after,
            after,
        });
    }
    Ok(images)
}

fn apply(
    resolved: &Resolved,
    journal: &Journal,
    raw: &[u8],
    dry_run: bool,
) -> Result<Vec<ConfigurationChange>> {
    let initial = validate_state(resolved, journal)?;
    let manifest = if matches!(journal.operation, Operation::Patch) {
        documents::manifest(None)?
    } else {
        let image = journal
            .edits
            .iter()
            .zip(&initial)
            .find(|(edit, _)| edit.role == EditKind::Manifest)
            .map(|(_, image)| image)
            .ok_or_else(|| invalid("The native journal is missing its manifest image."))?;
        documents::manifest(match journal.operation {
            Operation::Configure => image.after.as_deref(),
            Operation::Restore | Operation::RestoreFields => image.before.as_deref(),
            Operation::Patch => unreachable!(),
        })?
    };
    let changes = journal
        .edits
        .iter()
        .zip(&initial)
        .filter_map(|(edit, image)| {
            let client = match edit.role {
                EditKind::Target(client) => client,
                EditKind::ImageSkill => Client::Codex,
                _ => return None,
            };
            Some(ConfigurationChange {
                client,
                path: image.path.clone(),
                changed: image.changed,
                backup: manifest
                    .entries
                    .get(&client)
                    .filter(|entry| !entry.created)
                    .map(|_| resolved.backup.join(client.original())),
                action: match journal.operation {
                    Operation::Configure => "recover configure",
                    Operation::Restore => "recover restore",
                    Operation::RestoreFields => "recover owned routing fields",
                    Operation::Patch if edit.role == EditKind::ImageSkill => "recover image skill",
                    Operation::Patch => "recover routing patch",
                }
                .into(),
            })
        })
        .collect();
    if dry_run {
        return Ok(changes);
    }
    for index in 0..journal.edits.len() {
        checkpoint(&format!("before-edit:{index}"))?;
        // Revalidate the whole transaction so an external change in another
        // selected file does not permit subsequent edits to continue.
        let mut images = validate_state(resolved, journal)?;
        let image = images.swap_remove(index);
        apply_one(resolved, journal, index, &image)?;
        checkpoint(&format!("edit:{index}"))?;
    }
    let completed = validate_state(resolved, journal)?;
    if completed.iter().any(|image| image.changed) {
        return Err(conflict(
            "The transaction did not reach its complete post-image.",
        ));
    }
    for (index, image) in completed.iter().enumerate() {
        for (suffix, expected) in [
            ("displaced", image.before.as_deref()),
            ("tmp", image.after.as_deref()),
        ] {
            let path = journal.staged(&image.path, index, suffix)?;
            if let Some(bytes) = filesystem::read(&path)? {
                if !expected.is_some_and(|expected| expected.starts_with(&bytes)) {
                    return Err(conflict("Unknown transaction artifacts were preserved."));
                }
                filesystem::remove_known(&path, &bytes)?;
            }
        }
    }
    checkpoint("before-retire-journal")?;
    filesystem::remove_known(&resolved.backup.join(JOURNAL), raw)?;
    checkpoint("retired-journal")?;
    for (index, image) in completed.iter().enumerate() {
        for (side, bytes) in [
            ("before", image.before.as_deref()),
            ("after", image.after.as_deref()),
        ] {
            if let Some(bytes) = bytes {
                filesystem::remove_known(&journal.copy(&resolved.backup, index, side), bytes)?;
            }
        }
        filesystem::remove_known(&journal.done(&resolved.backup, index), &[])?;
    }
    std::fs::remove_dir(journal.directory(&resolved.backup))
        .map_err(|error| io_error("remove an empty completed recovery directory", &error))?;
    Ok(changes)
}

fn apply_one(resolved: &Resolved, journal: &Journal, index: usize, image: &Images) -> Result<()> {
    let role = journal.edits[index].role;
    let target = &image.path;
    let staged = journal.staged(target, index, "tmp")?;
    let displaced = journal.staged(target, index, "displaced")?;
    let mut current = filesystem::read(target)?;
    #[cfg(target_os = "macos")]
    if image.before.is_some()
        && image.before != image.after
        && current == image.after
        && filesystem::read(&staged)? == image.before
        && filesystem::read(&displaced)?.is_none()
    {
        filesystem::finish_replace(target, &staged, &displaced)?;
        checkpoint(&format!("mac-replace:finished:{index}"))?;
    }
    if current != image.after {
        if current.is_none() && image.before.is_some() && image.after.is_some() {
            filesystem::assert_current(&displaced, image.before.as_deref())?;
            filesystem::move_new(&displaced, target)?;
            checkpoint(&format!("restored-displaced:{index}"))?;
            current = filesystem::read(target)?;
        }
        if current != image.before {
            return Err(conflict(
                "A target changed before replacement; no overwrite was permitted.",
            ));
        }
        if let Some(old) = filesystem::read(&displaced)? {
            if Some(old.as_slice()) != image.before.as_deref() {
                return Err(conflict("An unknown displaced file was preserved."));
            }
            filesystem::remove_known(&displaced, &old)?;
        }
        if let Some(after) = &image.after {
            match filesystem::read(&staged)? {
                Some(bytes) if bytes == *after => {}
                Some(bytes) if after.starts_with(&bytes) => {
                    filesystem::remove_known(&staged, &bytes)?;
                    filesystem::write_private(&staged, after)?;
                }
                Some(_) => return Err(conflict("An unknown staging file was preserved.")),
                None => filesystem::write_private(&staged, after)?,
            }
            role.checkpoint("staged", index)?;
            filesystem::assert_current(target, image.before.as_deref())?;
            filesystem::assert_current(&staged, Some(after))?;
            role.checkpoint("before-replace", index)?;
            if image.before.is_some() {
                filesystem::replace(target, &staged, &displaced)?;
            } else {
                filesystem::move_new(&staged, target)?;
            }
        } else {
            filesystem::assert_current(target, image.before.as_deref())?;
            role.checkpoint("before-delete", index)?;
            filesystem::move_new(target, &displaced)?;
        }
        role.checkpoint("replaced", index)?;
    }
    filesystem::assert_current(target, image.after.as_deref())?;
    if let Some(bytes) = filesystem::read(&displaced)?
        && Some(bytes.as_slice()) != image.before.as_deref()
    {
        return Err(conflict(
            "A concurrent replacement was captured intact; recovery is blocked rather than deleting it.",
        ));
    }
    if !is_done(journal, &resolved.backup, index)? {
        filesystem::write_private(&journal.done(&resolved.backup, index), &[])?;
    }
    Ok(())
}

pub(super) fn recover_locked(
    paths: &ClientPaths,
    resolved: &Resolved,
    dry_run: bool,
) -> Result<Vec<ConfigurationChange>> {
    let Some((raw, journal)) = load(&resolved.backup)? else {
        return Ok(Vec::new());
    };
    let pending = journal_paths(paths, &resolved.backup, &journal)?;
    apply(&pending, &journal, &raw, dry_run)
}
