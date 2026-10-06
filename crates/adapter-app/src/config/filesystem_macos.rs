//! macOS filesystem effects, anchored to open, non-symlink directory descriptors.
use super::*;
use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use unicode_normalization::UnicodeNormalization;

unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd(fd: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
    fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
}

fn last(operation: &str) -> AdapterError {
    io_error(operation, &std::io::Error::last_os_error())
}

fn name(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| invalid("A configuration path contains NUL."))
}

/// Only the documented, root-owned system aliases are accepted. Other symlinks
/// remain prohibited, including symlinks underneath these directories.
pub(super) fn system_aliases(path: &Path) -> Result<PathBuf> {
    for (alias, destination) in [
        ("/tmp", "/private/tmp"),
        ("/var", "/private/var"),
        ("/etc", "/private/etc"),
    ] {
        if let Ok(tail) = path.strip_prefix(alias) {
            let metadata = std::fs::symlink_metadata(alias)
                .map_err(|error| io_error("inspect a macOS system path alias", &error))?;
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(alias)
                    .map_err(|error| io_error("read a macOS system path alias", &error))?;
                let expected = Path::new(destination);
                if metadata.uid() != 0
                    || (target != expected && target != expected.strip_prefix("/").unwrap())
                {
                    return Err(invalid(
                        "An unverified macOS system path alias was rejected.",
                    ));
                }
                return Ok(expected.join(tail));
            }
        }
    }
    Ok(path.to_owned())
}

fn from_fd(fd: RawFd, operation: &str) -> Result<File> {
    if fd == -1 {
        Err(last(operation))
    } else {
        // Every successful open/openat descriptor is transferred exactly once.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn sync_directory(directory: &File) -> Result<()> {
    directory
        .sync_all()
        .map_err(|error| io_error("flush a configuration directory", &error))
}

fn sync_file(file: &File) -> Result<()> {
    file.sync_all()
        .map_err(|error| io_error("flush a configuration file", &error))?;
    // F_FULLFSYNC also asks the device to flush its write cache. Some mounted
    // filesystems support only fsync; all other failures remain visible.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        let error = std::io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) {
            return Err(io_error("complete a configuration file flush", &error));
        }
    }
    Ok(())
}

fn make_private(file: &File, mode: libc::mode_t) -> Result<()> {
    // New objects must not inherit an ACL granting access beyond the mode bits.
    let acl = unsafe { acl_init(0) };
    if acl.is_null() {
        return Err(last("create private configuration security metadata"));
    }
    let status = unsafe { acl_set_fd(file.as_raw_fd(), acl) };
    let failure = (status == -1).then(|| last("remove inherited configuration ACLs"));
    unsafe { acl_free(acl) };
    if let Some(failure) = failure {
        return Err(failure);
    }
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } == -1 {
        return Err(last("set private configuration permissions"));
    }
    Ok(())
}

fn directory(path: &Path, create: bool) -> Result<File> {
    let path = system_aliases(path)?;
    if !path.is_absolute() {
        return Err(invalid("Configuration directory anchors must be absolute."));
    }
    let mut current = from_fd(
        unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        },
        "open the configuration filesystem root",
    )?;
    for component in path.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            _ => {
                return Err(invalid(
                    "A configuration directory contains unnormalized components.",
                ));
            }
        };
        let component = name(component)?;
        let created = if create {
            if unsafe { libc::mkdirat(current.as_raw_fd(), component.as_ptr(), 0o700) } == 0 {
                true
            } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
                false
            } else {
                return Err(last("create a configuration parent directory"));
            }
        } else {
            false
        };
        let next = from_fd(
            unsafe {
                libc::openat(
                    current.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            },
            "open a non-symlink configuration directory",
        )?;
        if created {
            make_private(&next, 0o700)?;
            sync_directory(&next)?;
            sync_directory(&current)?;
        }
        current = next;
    }
    Ok(current)
}

fn anchor(path: &Path, create: bool) -> Result<(File, CString)> {
    let path = system_aliases(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("A configuration path has no parent."))?;
    let basename = path
        .file_name()
        .ok_or_else(|| invalid("A configuration path has no filename."))?;
    Ok((directory(parent, create)?, name(basename)?))
}

fn open_at(directory: &File, basename: &CString, flags: libc::c_int) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            basename.as_ptr(),
            // A raced-in FIFO must not block before the regular-file check.
            // O_NONBLOCK does not change reads or writes of regular files.
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600 as libc::c_uint,
        )
    };
    if fd == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(file)
}

pub(super) fn open_read(path: &Path) -> std::io::Result<File> {
    let (parent, basename) = anchor(path, false).map_err(|error| {
        // Preserve ENOENT semantics for a missing parent; every other failure
        // still fails closed without following a symlink.
        let os = std::io::Error::last_os_error();
        if error.code == "configuration_io_error" && os.kind() == std::io::ErrorKind::NotFound {
            os
        } else {
            std::io::Error::other(error.message)
        }
    })?;
    open_at(&parent, &basename, libc::O_RDONLY)
}

pub(super) fn parents(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("A configuration path has no parent."))?;
    directory(parent, true).map(|_| ())
}

pub(super) fn create_private_file(path: &Path) -> Result<File> {
    let (parent, basename) = anchor(path, true)?;
    let file = open_at(
        &parent,
        &basename,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
    )
    .map_err(|error| io_error("create a private configuration image", &error))?;
    make_private(&file, 0o600)?;
    sync_directory(&parent)?;
    Ok(file)
}

pub(super) fn flush_private(file: &File, path: &Path) -> Result<()> {
    sync_file(file)?;
    sync_directory(&anchor(path, false)?.0)
}

fn registration_bytes(file: &File) -> Result<(std::fs::Metadata, Vec<u8>)> {
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect login recovery metadata", &error))?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 64 * 1024
    {
        return Err(invalid(
            "The login recovery registration is not a private, singly linked owned file.",
        ));
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error("read login recovery registration", &error))?;
    if bytes.len() > 64 * 1024 {
        return Err(invalid("The login recovery registration is oversized."));
    }
    Ok((metadata, bytes))
}

fn registration_temporary(basename: &CString) -> Result<CString> {
    let mut random = [0u8; 16];
    if unsafe { libc::getentropy(random.as_mut_ptr().cast(), random.len()) } != 0 {
        return Err(last("create a unique login recovery publication name"));
    }
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut name = b".".to_vec();
    name.extend_from_slice(basename.as_bytes());
    name.extend_from_slice(format!(".GitHubAdapter.{suffix}.tmp").as_bytes());
    CString::new(name).map_err(|_| invalid("Invalid login recovery filename."))
}

fn rename_registration(parent: &File, source: &CString, destination: &CString) -> Result<()> {
    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    } == -1
    {
        return Err(last(
            "move a login recovery registration without replacement",
        ));
    }
    sync_directory(parent)
}

/// Publish before routing is activated, and remove only the captured owned image.
pub(in crate::config) fn update_login_registration(
    path: &Path,
    expected: &[u8],
    arm: bool,
) -> Result<()> {
    if expected.len() > 64 * 1024 {
        return Err(invalid("The login recovery registration is oversized."));
    }
    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("The login recovery registration has no parent."))?;
    let library_path = parent_path
        .parent()
        .ok_or_else(|| invalid("The login recovery registration has no Library directory."))?;
    let home_path = library_path
        .parent()
        .ok_or_else(|| invalid("The login recovery registration has no home directory."))?;
    if parent_path.file_name() != Some(OsStr::new("LaunchAgents"))
        || library_path.file_name() != Some(OsStr::new("Library"))
    {
        return Err(invalid(
            "Login recovery must use the current user's Library/LaunchAgents directory.",
        ));
    }
    // Validate the existing home before creating anything. Every directory open
    // walks from / with O_NOFOLLOW, including HOME and Library.
    for directory_path in [home_path, library_path, parent_path] {
        let directory = match directory(directory_path, arm && directory_path != home_path) {
            Ok(directory) => directory,
            Err(error)
                if !arm
                    && error.code == "configuration_io_error"
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let metadata = directory
            .metadata()
            .map_err(|error| io_error("inspect a login recovery directory", &error))?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(invalid(
                "Login recovery directories must be owned by the current user and must not be writable by others.",
            ));
        }
    }
    let (parent, basename) = anchor(path, false)?;
    let existing = match open_at(&parent, &basename, libc::O_RDONLY) {
        Ok(file) => {
            let image = registration_bytes(&file)?;
            if arm && image.1 == expected {
                sync_file(&file)?;
                sync_directory(&parent)?;
            }
            Some(image)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(io_error(
                "open a non-symlink login recovery registration",
                &error,
            ));
        }
    };
    if arm {
        if let Some((_, bytes)) = existing {
            return if bytes == expected {
                Ok(())
            } else {
                Err(conflict(
                    "An unrelated login recovery registration was preserved.",
                ))
            };
        }
        let temporary = registration_temporary(&basename)?;
        let mut file = open_at(
            &parent,
            &temporary,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(|error| io_error("stage login recovery registration", &error))?;
        make_private(&file, 0o600)?;
        file.write_all(expected)
            .map_err(|error| io_error("write login recovery registration", &error))?;
        sync_file(&file)?;
        // A failed stage remains a private .tmp image; never unlink an object
        // which another writer may have replaced at the published pathname.
        rename_registration(&parent, &temporary, &basename)?;
        return Ok(());
    }
    let Some((metadata, bytes)) = existing else {
        return Ok(());
    };
    if bytes != expected {
        return Ok(());
    }
    checkpoint("mac-login:before-remove")?;
    let temporary = registration_temporary(&basename)?;
    rename_registration(&parent, &basename, &temporary)?;
    let captured = open_at(&parent, &temporary, libc::O_RDONLY)
        .map_err(|error| io_error("open the captured login recovery registration", &error))
        .and_then(|file| registration_bytes(&file));
    if !captured.as_ref().is_ok_and(|(current, bytes)| {
        current.dev() == metadata.dev() && current.ino() == metadata.ino() && bytes == expected
    }) {
        // A racing editor's actual image is preserved and restored if the
        // public name is still empty. An independently recreated name wins.
        let _ = rename_registration(&parent, &temporary, &basename);
        return Err(conflict(
            "Login recovery changed concurrently; its captured image was preserved.",
        ));
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) } == -1 {
        return Err(last(
            "remove the captured owned login recovery registration",
        ));
    }
    sync_directory(&parent)
}

pub(in crate::config) fn private_directory(path: &Path) -> Result<()> {
    let (parent, basename) = anchor(path, true)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), basename.as_ptr(), 0o700) } == -1 {
        return Err(last("create a uniquely owned private recovery directory"));
    }
    let created = directory(path, false)?;
    make_private(&created, 0o700)?;
    sync_directory(&created)?;
    sync_directory(&parent)
}

pub(super) fn remove(path: &Path) -> Result<()> {
    let (parent, basename) = anchor(path, false)?;
    let _regular = open_at(&parent, &basename, libc::O_RDONLY)
        .map_err(|error| io_error("verify a recovery file before removal", &error))?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), basename.as_ptr(), 0) } == -1 {
        return Err(last("remove a verified recovery file"));
    }
    sync_directory(&parent)
}

pub(in crate::config) fn move_new(source: &Path, destination: &Path) -> Result<()> {
    let (source_parent, source_name) = anchor(source, false)?;
    let (destination_parent, destination_name) = anchor(destination, false)?;
    let _regular = open_at(&source_parent, &source_name, libc::O_RDONLY)
        .map_err(|error| io_error("verify a file before exclusive rename", &error))?;
    if unsafe {
        libc::renameatx_np(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    } == -1
    {
        return Err(last("move a configuration file without replacement"));
    }
    sync_directory(&destination_parent)?;
    sync_directory(&source_parent)
}

/// After RENAME_SWAP the old target lives at `staged`. Finishing this separately
/// also permits the journal to recover an interruption immediately after swap.
pub(in crate::config) fn finish_replace(
    target: &Path,
    staged: &Path,
    displaced: &Path,
) -> Result<()> {
    let (target_parent, target_name) = anchor(target, false)?;
    let (staged_parent, staged_name) = anchor(staged, false)?;
    let current = open_at(&target_parent, &target_name, libc::O_RDWR)
        .map_err(|error| io_error("open the replacement configuration file", &error))?;
    let previous = open_at(&staged_parent, &staged_name, libc::O_RDONLY)
        .map_err(|error| io_error("open the captured configuration preimage", &error))?;
    sync_file(&previous)?;
    if unsafe {
        libc::fcopyfile(
            previous.as_raw_fd(),
            current.as_raw_fd(),
            std::ptr::null_mut(),
            libc::COPYFILE_METADATA,
        )
    } == -1
    {
        return Err(last("preserve replacement configuration security metadata"));
    }
    sync_file(&current)?;
    move_new(staged, displaced)
}

pub(in crate::config) fn replace(target: &Path, staged: &Path, displaced: &Path) -> Result<()> {
    check(target, false)?;
    check(staged, false)?;
    check(displaced, false)?;
    if read(displaced)?.is_some() {
        return Err(conflict("A displaced configuration file already exists."));
    }
    let (target_parent, target_name) = anchor(target, false)?;
    let (staged_parent, staged_name) = anchor(staged, false)?;
    let _previous = open_at(&target_parent, &target_name, libc::O_RDONLY)
        .map_err(|error| io_error("verify a replacement target", &error))?;
    let replacement = open_at(&staged_parent, &staged_name, libc::O_RDWR)
        .map_err(|error| io_error("verify a replacement image", &error))?;
    sync_file(&replacement)?;
    checkpoint("mac-replace:before-swap")?;
    if unsafe {
        libc::renameatx_np(
            target_parent.as_raw_fd(),
            target_name.as_ptr(),
            staged_parent.as_raw_fd(),
            staged_name.as_ptr(),
            libc::RENAME_SWAP,
        )
    } == -1
    {
        return Err(last("atomically exchange configuration files"));
    }
    sync_directory(&target_parent)?;
    sync_directory(&staged_parent)?;
    checkpoint("mac-replace:swapped")?;
    finish_replace(target, staged, displaced)
}

pub(super) fn path_key(path: &Path) -> Result<String> {
    let path = system_aliases(path)?;
    check(&path, path.is_dir())?;
    let mut existing = path.as_path();
    let mut missing = Vec::new();
    let mut canonical = loop {
        match std::fs::canonicalize(existing) {
            Ok(value) => break value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .ok_or_else(|| invalid("Invalid path ancestry."))?,
                );
                existing = existing
                    .parent()
                    .ok_or_else(|| invalid("Invalid path ancestry."))?;
            }
            Err(error) => return Err(io_error("canonicalize a configuration path", &error)),
        }
    };
    let volume_directory = if canonical.is_dir() {
        canonical.clone()
    } else {
        canonical.parent().unwrap().to_owned()
    };
    let volume = directory(&volume_directory, false)?;
    let case_sensitive = unsafe { libc::fpathconf(volume.as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
    if case_sensitive == -1 {
        return Err(last("determine configuration volume case sensitivity"));
    }
    for suffix in missing.into_iter().rev() {
        canonical.push(suffix);
    }
    let text = path_text(&canonical)?;
    Ok(if case_sensitive == 0 {
        // Upper/lower folding also merges forms such as final sigma. This is
        // intentionally conservative: a collision serializes paths rather than
        // allowing equivalent Unicode spellings to bypass a configuration lock.
        text.chars()
            .flat_map(char::to_uppercase)
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .nfd()
            .collect()
    } else {
        // APFS remains normalization-insensitive even when case-sensitive.
        text.nfd().collect()
    })
}

pub(in crate::config) struct MutexGuard {
    _file: File,
}

impl MutexGuard {
    pub(in crate::config) fn acquire(identity: &str) -> Result<Self> {
        // A fixed per-user location preserves locking across different HOME,
        // TMPDIR and backup overrides. Persistent empty files are never unlinked.
        let user = unsafe { libc::geteuid() };
        let root = PathBuf::from(format!("/private/tmp/GitHubAdapter.Configuration.{user}"));
        if let Err(failure) = private_directory(&root)
            && (failure.code != "configuration_io_error"
                || std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST))
        {
            return Err(failure);
        }
        let locked_directory = directory(&root, false)?;
        let metadata = locked_directory
            .metadata()
            .map_err(|error| io_error("inspect the configuration lock directory", &error))?;
        if metadata.uid() != user || metadata.mode() & 0o077 != 0 {
            return Err(invalid(
                "The configuration lock directory is not private to this user.",
            ));
        }
        make_private(&locked_directory, 0o700)?;
        let basename = CString::new(format!("{}.lock", hash(identity.as_bytes()))).unwrap();
        let file = open_at(&locked_directory, &basename, libc::O_RDWR | libc::O_CREAT)
            .map_err(|error| io_error("open a per-target configuration lock", &error))?;
        let metadata = file
            .metadata()
            .map_err(|error| io_error("inspect a configuration lock", &error))?;
        if metadata.uid() != user || metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err(invalid(
                "A configuration lock is not a private, singly linked file.",
            ));
        }
        make_private(&file, 0o600)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(conflict(
                    "Another setup/restore/recovery holds a target-path configuration lock.",
                ));
            }
            return Err(io_error("acquire a target-path configuration lock", &error));
        }
        Ok(Self { _file: file })
    }
}
