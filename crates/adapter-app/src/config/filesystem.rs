use super::*;
use sha2::{Digest, Sha256};
use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::path::Component;

pub(super) struct Resolved {
    pub targets: BTreeMap<Client, PathBuf>,
    pub backup: PathBuf,
}

pub(super) fn resolve(paths: &ClientPaths, clients: &[Client]) -> Result<Resolved> {
    validate_clients(clients)?;
    let backup = absolute(&paths.backup_dir, true)?;
    let mut targets: BTreeMap<Client, PathBuf> = BTreeMap::new();
    let backup_key = key(&backup)?;
    for client in clients {
        let path = paths
            .path(*client)
            .ok_or_else(|| invalid("A settings path is required for every selected client."))?;
        let path = absolute(path, false)?;
        let target_key = key(&path)?;
        if within(&target_key, &backup_key) || within(&backup_key, &target_key) {
            return Err(invalid(
                "Settings files and the backup directory must not contain one another.",
            ));
        }
        for other in targets.values() {
            let other_key = key(other)?;
            if within(&target_key, &other_key) || within(&other_key, &target_key) {
                return Err(invalid(
                    "Selected clients must use distinct, nonoverlapping settings paths.",
                ));
            }
        }
        targets.insert(*client, path);
    }
    Ok(Resolved { targets, backup })
}

fn within(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|tail| tail.starts_with(['\\', '/']))
}

pub(super) fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("Configuration paths must be representable as Unicode."))
}

pub(super) fn absolute(path: &Path, directory: bool) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(invalid("A configuration path must not be empty."));
    }
    let text = path_text(path)?;
    if text.contains('\0') {
        return Err(invalid("Configuration paths must not contain NUL."));
    }
    let expanded = if text == "~" {
        home()?
    } else if let Some(tail) = text.strip_prefix("~\\").or_else(|| text.strip_prefix("~/")) {
        home()?.join(tail)
    } else {
        path.to_path_buf()
    };
    let raw = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|error| io_error("resolve the working directory", &error))?
            .join(expanded)
    };
    #[cfg(target_os = "macos")]
    let raw = platform::system_aliases(&raw)?;
    check(&raw, directory)?;
    let mut normalized = PathBuf::new();
    for part in raw.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid("A configuration path escapes its root."));
                }
            }
            Component::Normal(name) => {
                #[cfg(windows)]
                {
                    let value = name
                        .to_str()
                        .ok_or_else(|| invalid("Invalid Unicode path."))?;
                    if value.contains(':') || value.ends_with([' ', '.']) {
                        return Err(invalid(
                            "Alternate streams and ambiguous Windows path components are unsupported.",
                        ));
                    }
                }
                normalized.push(name);
            }
            Component::Prefix(prefix) => {
                #[cfg(windows)]
                if matches!(
                    prefix.kind(),
                    std::path::Prefix::DeviceNS(_) | std::path::Prefix::Verbatim(_)
                ) {
                    return Err(invalid(
                        "Windows device namespace paths are not configuration files.",
                    ));
                }
                normalized.push(prefix.as_os_str());
            }
            Component::RootDir => normalized.push(part.as_os_str()),
        }
    }
    if !normalized.is_absolute() || normalized.file_name().is_none() {
        return Err(invalid(
            "Use an absolute file or non-root backup-directory path.",
        ));
    }
    check(&normalized, directory)?;
    Ok(normalized)
}

#[cfg(target_os = "macos")]
pub(super) fn key(path: &Path) -> Result<String> {
    platform::path_key(path)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn key(path: &Path) -> Result<String> {
    let mut current = path;
    let mut missing = Vec::new();
    let mut canonical = loop {
        match std::fs::canonicalize(current) {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    current
                        .file_name()
                        .ok_or_else(|| invalid("Invalid path ancestry."))?,
                );
                current = current
                    .parent()
                    .ok_or_else(|| invalid("Invalid path ancestry."))?;
            }
            Err(error) => return Err(io_error("canonicalize a configuration path", &error)),
        }
    };
    for name in missing.into_iter().rev() {
        canonical.push(name);
    }
    let text = path_text(&canonical)?;
    #[cfg(windows)]
    {
        let text = if let Some(tail) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{tail}")
        } else {
            text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
        };
        Ok(text.to_lowercase())
    }
    #[cfg(not(windows))]
    {
        Ok(text)
    }
}

fn reparse(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub(super) fn check(path: &Path, directory: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    let normalized = platform::system_aliases(path)?;
    #[cfg(target_os = "macos")]
    let path = normalized.as_path();
    for current in path.ancestors() {
        let metadata = match std::fs::symlink_metadata(current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error("inspect a configuration path", &error)),
        };
        if reparse(&metadata) {
            return Err(invalid(
                "Settings and recovery paths must not contain symlinks or reparse points.",
            ));
        }
        if directory || current != path {
            if !metadata.is_dir() {
                return Err(invalid(
                    "A configuration parent or backup path is not a directory.",
                ));
            }
        } else if !metadata.is_file() {
            return Err(invalid(
                "A settings or recovery target is not a regular file.",
            ));
        }
    }
    Ok(())
}

pub(super) fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    check(path, false)?;
    #[cfg(not(target_os = "macos"))]
    let mut options = File::options();
    #[cfg(not(target_os = "macos"))]
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(target_os = "macos"))]
    let opened = options.open(path);
    #[cfg(target_os = "macos")]
    let opened = platform::open_read(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("open a settings or recovery file", &error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect an open configuration file", &error))?;
    if !metadata.is_file() || reparse(&metadata) {
        return Err(invalid(
            "An opened configuration file is not a regular non-reparse file.",
        ));
    }
    let mut raw = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|error| io_error("read a settings or recovery file", &error))?;
    if raw.len() > MAX_BYTES {
        return Err(invalid(
            "A settings or recovery file exceeds the 8 MiB safety limit.",
        ));
    }
    Ok(Some(raw))
}

pub(super) fn hash(raw: &[u8]) -> String {
    format!("{:x}", Sha256::digest(raw))
}

pub(super) fn assert_current(path: &Path, expected: Option<&[u8]>) -> Result<()> {
    if read(path)?.as_deref() != expected {
        return Err(conflict(
            "A settings or recovery file changed concurrently; no overwrite is permitted.",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn parents(path: &Path) -> Result<()> {
    platform::parents(path)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn parents(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("A file path has no parent."))?;
    check(parent, true)?;
    std::fs::create_dir_all(parent)
        .map_err(|error| io_error("create configuration parent directories", &error))?;
    check(parent, true)
}

pub(super) fn write_private(path: &Path, raw: &[u8]) -> Result<()> {
    if raw.len() > MAX_BYTES {
        return Err(invalid("A recovery image exceeds the 8 MiB safety limit."));
    }
    check(path, false)?;
    parents(path)?;
    let mut file = platform::create_private_file(path)?;
    file.write_all(raw)
        .and_then(|_| file.sync_all())
        .map_err(|error| io_error("flush a private configuration image", &error))?;
    #[cfg(target_os = "macos")]
    platform::flush_private(&file, path)?;
    Ok(())
}

pub(super) fn remove_known(path: &Path, expected: &[u8]) -> Result<()> {
    assert_current(path, Some(expected))?;
    #[cfg(target_os = "macos")]
    {
        platform::remove(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::remove_file(path)
            .map_err(|error| io_error("remove a verified recovery file", &error))
    }
}

#[cfg(target_os = "macos")]
pub(super) use platform::finish_replace;
#[cfg(target_os = "macos")]
pub(super) use platform::update_login_registration;
pub(super) use platform::{MutexGuard, move_new, private_directory, replace};

#[cfg(target_os = "macos")]
#[path = "filesystem_macos.rs"]
mod platform;

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE,
        GetLengthSid, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
        OBJECT_INHERIT_ACE, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
        MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, GetCurrentProcess, OpenProcessToken, ReleaseMutex, WaitForSingleObject,
    };

    fn wide(path: &Path) -> Result<Vec<u16>> {
        // Only the native I/O representation changes; display and lock keys do not.
        let path = std::path::absolute(path)
            .map_err(|error| io_error("resolve a native configuration path", &error))?;
        let (prefix, skip) = match path.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                std::path::Prefix::Disk(_) => (r"\\?\", 0),
                std::path::Prefix::UNC(_, _) => (r"\\?\UNC\", 2),
                std::path::Prefix::VerbatimDisk(_) | std::path::Prefix::VerbatimUNC(_, _) => {
                    ("", 0)
                }
                _ => return Err(invalid("Unsupported native configuration path prefix.")),
            },
            _ => {
                return Err(invalid(
                    "Native configuration I/O requires an absolute path.",
                ));
            }
        };
        Ok(prefix
            .encode_utf16()
            .chain(path.as_os_str().encode_wide().skip(skip))
            .chain(Some(0))
            .collect())
    }

    fn last(operation: &str) -> AdapterError {
        io_error(operation, &std::io::Error::last_os_error())
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    struct PrivateSecurity {
        descriptor: Box<SECURITY_DESCRIPTOR>,
        _acl: Vec<u32>,
    }

    impl PrivateSecurity {
        fn new() -> Result<Self> {
            let mut token = ptr::null_mut();
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                return Err(last("read the current-user security identity"));
            }
            let token = Handle(token);
            let mut size = 0;
            unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut size) };
            if size == 0 || size > 64 * 1024 {
                return Err(last("size the current-user security identity"));
            }
            let mut user = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            if unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    user.as_mut_ptr().cast(),
                    size,
                    &mut size,
                )
            } == 0
            {
                return Err(last("read the current-user security identity"));
            }
            // TOKEN_USER and its SID are in this aligned, live token buffer.
            let sid = unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
            let sid_size = unsafe { GetLengthSid(sid) } as usize;
            if sid_size == 0 || sid_size > 1024 {
                return Err(invalid("The current-user SID has an invalid size."));
            }
            let acl_size =
                std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>() + sid_size
                    - 4;
            let mut acl = vec![0_u32; acl_size.div_ceil(4)];
            let acl_pointer = acl.as_mut_ptr().cast::<ACL>();
            let mut descriptor: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { std::mem::zeroed() });
            let descriptor_pointer = (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast();
            // The single inheritable ACE grants the current user full access.
            // A protected DACL prevents broad parent ACLs from exposing copies.
            unsafe {
                if InitializeAcl(acl_pointer, acl_size as u32, ACL_REVISION) == 0
                    || AddAccessAllowedAceEx(
                        acl_pointer,
                        ACL_REVISION,
                        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                        GENERIC_ALL,
                        sid,
                    ) == 0
                    || InitializeSecurityDescriptor(descriptor_pointer, 1) == 0
                    || SetSecurityDescriptorDacl(descriptor_pointer, 1, acl_pointer, 0) == 0
                    || SetSecurityDescriptorControl(
                        descriptor_pointer,
                        SE_DACL_PROTECTED,
                        SE_DACL_PROTECTED,
                    ) == 0
                {
                    return Err(last("create private recovery security metadata"));
                }
            }
            Ok(Self {
                descriptor,
                _acl: acl,
            })
        }

        fn attributes(&mut self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: (&mut *self.descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                bInheritHandle: 0,
            }
        }
    }

    pub(super) fn create_private_file(path: &Path) -> Result<File> {
        let mut security = PrivateSecurity::new()?;
        let attributes = security.attributes();
        let path = wide(path)?;
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last("create a private recovery file without replacement"));
        }
        // The successful CreateFileW handle is transferred to File exactly once.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    pub(in crate::config) fn private_directory(path: &Path) -> Result<()> {
        check(path, true)?;
        parents(path)?;
        let mut security = PrivateSecurity::new()?;
        let attributes = security.attributes();
        let path = wide(path)?;
        if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
            return Err(last("create a private, uniquely owned recovery directory"));
        }
        Ok(())
    }

    pub(in crate::config) fn move_new(source: &Path, destination: &Path) -> Result<()> {
        check(source, false)?;
        check(destination, false)?;
        let source = wide(source)?;
        let destination = wide(destination)?;
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(last(
                "move a configuration file without replacing another file",
            ));
        }
        Ok(())
    }

    pub(in crate::config) fn replace(target: &Path, staged: &Path, displaced: &Path) -> Result<()> {
        check(target, false)?;
        check(staged, false)?;
        check(displaced, false)?;
        let target = wide(target)?;
        let staged = wide(staged)?;
        let displaced = wide(displaced)?;
        // Flags zero deliberately do not ignore ACL/metadata merge failures.
        // Keep the displaced file until its preimage hash has been verified.
        if unsafe {
            ReplaceFileW(
                target.as_ptr(),
                staged.as_ptr(),
                displaced.as_ptr(),
                0,
                ptr::null(),
                ptr::null(),
            )
        } == 0
        {
            return Err(last(
                "atomically replace a file while preserving its security metadata",
            ));
        }
        Ok(())
    }

    pub(in crate::config) struct MutexGuard {
        handle: Handle,
    }

    impl MutexGuard {
        pub(in crate::config) fn acquire(identity: &str) -> Result<Self> {
            let name: Vec<u16> = format!(
                r"Global\GitHubAdapter.Configuration.{}",
                hash(identity.as_bytes())
            )
            .encode_utf16()
            .chain(Some(0))
            .collect();
            let mut security = PrivateSecurity::new()?;
            let attributes = security.attributes();
            let handle = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
            if handle.is_null() {
                return Err(last("open a per-target configuration mutex"));
            }
            let handle = Handle(handle);
            match unsafe { WaitForSingleObject(handle.0, 0) } {
                WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle }),
                WAIT_TIMEOUT => Err(conflict(
                    "Another native setup/restore/recovery holds a target-path configuration lock.",
                )),
                _ => Err(last("acquire a target-path configuration mutex")),
            }
        }
    }

    impl Drop for MutexGuard {
        fn drop(&mut self) {
            unsafe { ReleaseMutex(self.handle.0) };
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;

    fn unsupported() -> AdapterError {
        AdapterError::new(
            501,
            "unsupported_platform",
            "Native recoverable configuration and synchronized inspection currently require Windows.",
        )
    }

    pub(super) fn create_private_file(_: &Path) -> Result<File> {
        Err(unsupported())
    }
    pub(in crate::config) fn private_directory(_: &Path) -> Result<()> {
        Err(unsupported())
    }
    pub(in crate::config) fn move_new(_: &Path, _: &Path) -> Result<()> {
        Err(unsupported())
    }
    pub(in crate::config) fn replace(_: &Path, _: &Path, _: &Path) -> Result<()> {
        Err(unsupported())
    }
    pub(in crate::config) struct MutexGuard;
    impl MutexGuard {
        pub(in crate::config) fn acquire(_: &str) -> Result<Self> {
            Err(unsupported())
        }
    }
}
