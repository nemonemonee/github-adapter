//! Own temporary Codex configuration for the lifetime of the native host.
//! A pipe-watching companion survives an ordinary host crash; a one-shot
//! current-user logon entry covers logoff/power loss. Neither runs when off.
use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use crate::config::{ClientPaths, ConfigurationChange, ConfigureOptions, on_demand as mode};
use adapter_protocol::{AdapterError, Result};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::Registration;

fn error(message: &str) -> AdapterError {
    AdapterError::new(503, "on_demand_recovery", message)
}

#[derive(Default)]
pub struct Session {
    active: Mutex<Option<Guard>>,
}

struct Guard {
    owner: mode::Owner,
    registration: Registration,
    input: Option<ChildStdin>,
    finished: Arc<AtomicBool>,
    exited: mpsc::Receiver<()>,
}

impl Session {
    pub fn configure(
        &self,
        paths: ClientPaths,
        endpoint: String,
        options: ConfigureOptions,
    ) -> Result<Vec<ConfigurationChange>> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| error("On-demand ownership is unavailable."))?;
        if active.is_some() {
            return Err(error("This host already owns an on-demand activation."));
        }
        let executable = companion()?;
        if let Some(previous) = mode::load(&paths)? {
            if owner_alive(&previous)? {
                return Err(error("Another running host owns these Codex settings."));
            }
            mode::restore(&previous)?;
            Registration::new(&executable, &previous)?.disarm()?;
        }
        let (owner, registration) = prepare_registration(&executable, &paths, &endpoint, &options)?;
        if let Err(failure) = registration.arm() {
            let _ = mode::restore(&owner);
            return Err(failure);
        }
        let guard = match spawn_guard(&executable, owner.clone(), registration) {
            Ok(guard) => guard,
            Err(failure) => {
                if mode::restore(&owner).is_ok() {
                    let _ = Registration::new(&executable, &owner)?.disarm();
                }
                return Err(failure);
            }
        };
        *active = Some(guard);
        let result = mode::activate(&owner, &endpoint, &options);
        drop(active);
        if result.is_err() {
            let _ = self.finish();
        }
        result
    }

    pub fn companion_failed(&self) -> bool {
        self.active.try_lock().ok().is_some_and(|active| {
            active
                .as_ref()
                .is_some_and(|guard| guard.finished.load(Ordering::Acquire))
        })
    }

    pub fn finish(&self) -> Result<()> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| error("On-demand ownership is unavailable."))?;
        let Some(mut guard) = active.take() else {
            return Ok(());
        };
        let restored = mode::restore(&guard.owner);
        let result = restored.and_then(|_| guard.registration.disarm());
        // Closing the private pipe also asks the companion to retry any failed
        // recovery; it must never depend on the host executing an exit handler.
        guard.input.take();
        let _ = guard.exited.recv_timeout(Duration::from_secs(5));
        result
    }
}

fn prepare_registration(
    executable: &Path,
    paths: &ClientPaths,
    endpoint: &str,
    options: &ConfigureOptions,
) -> Result<(mode::Owner, Registration)> {
    let plan = mode::plan(
        paths,
        endpoint,
        options,
        std::process::id(),
        process_created(std::process::id())?
            .ok_or_else(|| error("Cannot identify the current host."))?,
    )?;
    let registration = Registration::new(executable, plan.owner())?;
    Ok((plan.publish()?, registration))
}

fn companion() -> Result<PathBuf> {
    let current =
        std::env::current_exe().map_err(|_| error("Cannot locate the native executable pair."))?;
    let path = current
        .parent()
        .ok_or_else(|| error("The native executable has no directory."))?
        .join(if cfg!(windows) {
            "github-adapter.exe"
        } else {
            "github-adapter"
        });
    if !path.is_file() {
        return Err(error(
            "The paired native CLI is missing; normal settings were not changed.",
        ));
    }
    Ok(path)
}

fn spawn_guard(executable: &Path, owner: mode::Owner, registration: Registration) -> Result<Guard> {
    let mut command = Command::new(executable);
    command
        .args([
            OsString::from("--mode-guardian"),
            owner.target.clone().into(),
            owner.backup_dir.clone().into(),
            owner.id.clone().into(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW; no elevation or job bypass.
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|_| error("The independent recovery companion could not start."))?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| error("The recovery pipe is unavailable."))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| error("The recovery acknowledgment is unavailable."))?;
    let (ready_send, ready_receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(output).take(32);
        let mut line = String::new();
        let ready = reader.read_line(&mut line).is_ok() && line == "READY\n";
        let _ = ready_send.send(ready);
    });
    if ready_receive.recv_timeout(Duration::from_secs(5)) != Ok(true) {
        drop(input);
        // This is only the child created above, before any routing mutation.
        let _ = child.kill();
        let _ = child.wait();
        return Err(error(
            "The recovery companion did not acknowledge readiness; normal routing was not activated.",
        ));
    }
    let finished = Arc::new(AtomicBool::new(false));
    let observed = finished.clone();
    let (exit_send, exited) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = child.wait();
        observed.store(true, Ordering::Release);
        let _ = exit_send.send(());
    });
    Ok(Guard {
        owner,
        registration,
        input: Some(input),
        finished,
        exited,
    })
}

pub fn auxiliary(arguments: &[OsString]) -> Option<Result<i32>> {
    let mode_arg = arguments.get(1)?.to_str()?;
    if mode_arg == "--restore-normal" {
        return Some((|| {
            if arguments.len() != 2 {
                return Err(error("Invalid normal-routing recovery invocation."));
            }
            let paths =
                crate::config::default_paths(None, None, None, &[crate::config::Client::Codex])?;
            restore_normal_if_stopped(paths, 5001)?;
            Ok(0)
        })());
    }
    if !matches!(
        mode_arg,
        "--mode-guardian" | "--recover-mode" | "--logon-recovery"
    ) {
        return None;
    }
    Some((|| {
        let owner = if mode_arg != "--mode-guardian" {
            if arguments.len() != 3 {
                return Err(error("Invalid native logon recovery invocation."));
            }
            let Some(owner) = mode::load_directory(Path::new(&arguments[2]))? else {
                #[cfg(target_os = "macos")]
                if mode_arg == "--logon-recovery" {
                    Registration::for_directory(&companion()?, Path::new(&arguments[2]))?
                        .disarm()?;
                }
                return Ok(0);
            };
            owner
        } else {
            if arguments.len() != 5 {
                return Err(error("Invalid native recovery invocation."));
            }
            let paths = ClientPaths {
                codex_config: Some(PathBuf::from(&arguments[2])),
                claude_settings: None,
                backup_dir: PathBuf::from(&arguments[3]),
            };
            let Some(owner) = mode::load(&paths)? else {
                return Ok(0);
            };
            if arguments[4].to_str() != Some(owner.id.as_str()) {
                return Ok(0);
            }
            owner
        };
        if mode_arg == "--mode-guardian" {
            if !owner_alive(&owner)? {
                return recover(&owner, false);
            }
            println!("READY");
            std::io::stdout()
                .flush()
                .map_err(|_| error("Could not acknowledge recovery readiness."))?;
            let mut input = std::io::stdin().lock();
            let mut byte = [0u8; 1];
            while input
                .read(&mut byte)
                .map_err(|_| error("The host recovery pipe failed."))?
                != 0
            {}
        } else if owner_alive(&owner)? {
            // Never restore underneath a live owner in another login session.
            return Ok(0);
        }
        recover(&owner, mode_arg == "--logon-recovery")
    })())
}

fn recover(owner: &mode::Owner, from_logon: bool) -> Result<i32> {
    for attempt in 0..5 {
        match mode::restore(owner) {
            Ok(_) => {
                // RunOnce removes its own entry after this GUI helper exits.
                if !from_logon || cfg!(target_os = "macos") {
                    Registration::new(&companion()?, owner)?.disarm()?;
                }
                return Ok(0);
            }
            Err(failure) if attempt == 4 => return Err(failure),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
    unreachable!()
}

fn owner_alive(owner: &mode::Owner) -> Result<bool> {
    Ok(process_created(owner.pid)? == Some(owner.process_created))
}

#[cfg(windows)]
fn process_created(pid: u32) -> Result<Option<u64>> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, WAIT_OBJECT_0},
        System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    };
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return if std::io::Error::last_os_error().raw_os_error() == Some(87) {
            Ok(None)
        } else {
            Err(error(
                "Cannot verify the configuration owner's process; recovery was not forced.",
            ))
        };
    }
    let result = if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
        Ok(None)
    } else {
        let mut created: FILETIME = unsafe { std::mem::zeroed() };
        let mut exit = created;
        let mut kernel = created;
        let mut user = created;
        if unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) } == 0
        {
            Err(error(
                "Cannot verify the configuration owner's creation time.",
            ))
        } else {
            Ok(Some(
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            ))
        }
    };
    unsafe {
        CloseHandle(handle);
    }
    result
}

#[cfg(target_os = "macos")]
pub(crate) fn process_created(pid: u32) -> Result<Option<u64>> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    let received = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if received == 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(error(
                "Cannot verify the configuration owner's process; recovery was not forced.",
            )),
        };
    }
    if received != size || info.pbi_uid != unsafe { libc::geteuid() } {
        return Err(error("Cannot verify the configuration owner's identity."));
    }
    // Zombies no longer own settings even if their PID remains in the process table.
    if info.pbi_status == libc::SZOMB {
        return Ok(None);
    }
    info.pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(info.pbi_start_tvusec))
        .map(Some)
        .ok_or_else(|| error("Invalid configuration owner creation time."))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn process_created(_: u32) -> Result<Option<u64>> {
    Err(error(
        "On-demand process recovery requires Windows or macOS.",
    ))
}

#[cfg(not(target_os = "macos"))]
struct Registration {
    name: String,
    command: String,
}
#[cfg(not(target_os = "macos"))]
impl Registration {
    fn new(executable: &Path, owner: &mode::Owner) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let identity = format!(
            "{:x}",
            Sha256::digest(owner.target.to_lowercase().as_bytes())
        );
        // Use the GUI-subsystem executable at logon, so recovery never opens a console.
        let executable = executable
            .parent()
            .ok_or_else(|| error("The recovery executable has no directory."))?
            .join("github-adapter-host.exe");
        if !executable.is_file() {
            return Err(error("The paired GUI recovery executable is missing."));
        }
        let executable = executable
            .to_str()
            .ok_or_else(|| error("The recovery executable path is not valid Unicode."))?;
        let directory = mode::mode_paths(&owner.paths()).backup_dir;
        let directory = directory
            .to_str()
            .ok_or_else(|| error("Invalid recovery directory."))?;
        let command = [executable, "--logon-recovery", directory]
            .map(quote_argument)
            .join(" ");
        if command.encode_utf16().count() > 260 {
            return Err(error(
                "The one-shot recovery command exceeds Windows' length limit; use a shorter installation or backup path.",
            ));
        }
        Ok(Self {
            name: format!(
                "!GitHubAdapter.RecoverMode.{}.{}",
                &identity[..16],
                &owner.id[..16]
            ),
            command,
        })
    }
    fn arm(&self) -> Result<()> {
        registry::update(&self.name, &self.command, true)
    }
    fn disarm(&self) -> Result<()> {
        registry::update(&self.name, &self.command, false)
    }
}

#[cfg(any(not(target_os = "macos"), test))]
fn quote_argument(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        result.push(ch);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

#[cfg(windows)]
mod registry {
    use super::*;
    use windows_sys::Win32::System::Registry::*;
    const RUN_ONCE: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\RunOnce";
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    pub(super) fn update(name: &str, command: &str, arm: bool) -> Result<()> {
        at(RUN_ONCE, name, command, arm)
    }
    pub(super) fn at(root: &str, name: &str, command: &str, arm: bool) -> Result<()> {
        let mut handle = std::ptr::null_mut();
        let status = if arm {
            unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(root).as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_QUERY_VALUE | KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut handle,
                    std::ptr::null_mut(),
                )
            }
        } else {
            unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    wide(root).as_ptr(),
                    0,
                    KEY_QUERY_VALUE | KEY_SET_VALUE,
                    &mut handle,
                )
            }
        };
        if !arm && status == 2 {
            return Ok(());
        }
        if status != 0 {
            return Err(error(
                "Cannot register current-user one-shot recovery; normal routing was not activated.",
            ));
        }
        let key = Key(handle);
        let name = wide(name);
        let expected = wide(command);
        let mut kind = 0;
        let mut size = 0;
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            )
        };
        let current = if status == 2 {
            None
        } else {
            if status != 0 || kind != REG_SZ || size > 64 * 1024 || size % 2 != 0 {
                return Err(error(
                    "The one-shot recovery registration is not an expected string.",
                ));
            }
            let mut value = vec![0u16; size as usize / 2];
            if unsafe {
                RegQueryValueExW(
                    key.0,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    value.as_mut_ptr().cast(),
                    &mut size,
                )
            } != 0
            {
                return Err(error("Cannot read the one-shot recovery registration."));
            }
            Some(value)
        };
        if arm {
            if current.as_ref().is_some_and(|value| *value != expected) {
                return Err(error(
                    "Another recovery registration owns this settings path; it was preserved.",
                ));
            }
            if unsafe {
                RegSetValueExW(
                    key.0,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    expected.as_ptr().cast(),
                    (expected.len() * 2) as u32,
                )
            } != 0
            {
                return Err(error(
                    "Cannot arm one-shot recovery; normal routing was not activated.",
                ));
            }
        } else if current.as_ref() == Some(&expected)
            && unsafe { RegDeleteValueW(key.0, name.as_ptr()) } != 0
        {
            return Err(error(
                "Normal routing is restored, but its one-shot recovery entry could not be removed.",
            ));
        }
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod registry {
    use super::*;
    pub(super) fn update(_: &str, _: &str, _: bool) -> Result<()> {
        Err(error("On-demand logon recovery requires Windows."))
    }
}

/// Installation/first-start migration. Authentication is not required to get
/// back to normal, and an occupied socket or live owner is never displaced.
pub fn restore_normal_if_stopped(paths: ClientPaths, requested_port: u16) -> Result<()> {
    if let Some(previous) = mode::load(&paths)? {
        if owner_alive(&previous)? {
            return Err(error(
                "Quit the running GitHub Adapter before restoring normal routing.",
            ));
        }
        mode::restore(&previous)?;
        Registration::new(&companion()?, &previous)?.disarm()?;
    }
    let Some((endpoint, address)) = mode::legacy_local_route(&paths, requested_port)? else {
        return Ok(());
    };
    let _exclusive = crate::listener::bind_loopback(address)?;
    let session = Session::default();
    session.configure(paths, endpoint, ConfigureOptions::default())?;
    session.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn rejected_recovery_registration_does_not_publish_a_live_owner() {
        let root = tempfile::Builder::new()
            .prefix("mode-plan-")
            .tempdir()
            .unwrap();
        let target = root.path().join("config.toml");
        let original = b"model_provider = 'openai'\r\n";
        std::fs::write(&target, original).unwrap();
        let paths = ClientPaths {
            codex_config: Some(target.clone()),
            claude_settings: None,
            backup_dir: root.path().join("b".repeat(140)),
        };
        let executable = root.path().join("github-adapter.exe");
        // Existence-only companions: registration validation must not execute either one.
        std::fs::write(&executable, b"fixture").unwrap();
        std::fs::write(root.path().join("github-adapter-host.exe"), b"fixture").unwrap();
        let first = prepare_registration(
            &executable,
            &paths,
            "http://127.0.0.1:53127",
            &ConfigureOptions::default(),
        );
        let second = prepare_registration(
            &executable,
            &paths,
            "http://127.0.0.1:53127",
            &ConfigureOptions::default(),
        );
        let leftover = mode::load(&paths).unwrap();
        if let Some(owner) = &leftover {
            mode::restore(owner).unwrap();
        }
        let message = |result: Result<(mode::Owner, Registration)>| match result {
            Ok(_) => panic!("The deliberately oversized recovery command was accepted."),
            Err(error) => error.message,
        };
        assert!(message(first).contains("length limit"));
        assert!(message(second).contains("length limit"));
        assert!(
            leftover.is_none(),
            "Failed preflight must not leave a live ownership receipt."
        );
        assert_eq!(std::fs::read(target).unwrap(), original);
    }

    #[test]
    fn recovery_arguments_quote_spaces_unicode_quotes_and_trailing_slashes() {
        assert_eq!(quote_argument("C:\\two words\\"), "\"C:\\two words\\\\\"");
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_argument("名字"), "\"名字\"");
    }
    #[cfg(windows)]
    #[test]
    fn one_shot_registration_preserves_unknown_values_and_only_removes_its_own() {
        use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyW};
        let root = format!(
            "Software\\GitHubAdapter\\Tests\\Mode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        registry::at(&root, "test", "other command", true).unwrap();
        assert!(registry::at(&root, "test", "our command", true).is_err());
        registry::at(&root, "test", "our command", false).unwrap();
        registry::at(&root, "test", "other command", false).unwrap();
        registry::at(&root, "test", "our command", true).unwrap();
        registry::at(&root, "test", "our command", false).unwrap();
        let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
        assert_eq!(
            unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide.as_ptr()) },
            0
        );
    }
}
