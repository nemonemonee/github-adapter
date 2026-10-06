use std::ffi::OsString;

pub fn enabled(arguments: &[OsString]) -> bool {
    interactive_start(arguments, owns_console())
}

fn interactive_start(arguments: &[OsString], exclusive_console: bool) -> bool {
    exclusive_console
        && (arguments.len() <= 1 || arguments.get(1).is_some_and(|value| value == "start"))
}

#[cfg(windows)]
fn owns_console() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;

    let mut processes = [0; 2];
    let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) };
    // A shortcut's temporary console disappears on exit; a shell's console must stay nonmodal.
    count == 1 && processes[0] == std::process::id()
}

#[cfg(not(windows))]
fn owns_console() -> bool {
    false
}

#[cfg(windows)]
pub fn show() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    let title: Vec<_> = "GitHub Adapter - startup needs attention\0"
        .encode_utf16()
        .collect();
    let message: Vec<_> = concat!(
        "GitHub Adapter could not confirm that startup and Codex activation completed.\n\n",
        "To inspect the host, open Windows Terminal (PowerShell) and run:\n\n",
        "& \"$env:LOCALAPPDATA\\GitHubAdapter\\bin\\github-adapter.exe\" status\n\n",
        "If no host is running, replace status with doctor for configuration diagnostics.\n",
        "Start the tray application using the GitHub Adapter desktop shortcut.\0",
    )
    .encode_utf16()
    .collect();
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
    if result == 0 {
        eprintln!("startup_dialog_unavailable: Windows could not display the startup diagnostic.");
    }
}

#[cfg(target_os = "macos")]
pub fn show() {
    crate::host::show_macos_startup_error();
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn show() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn shortcut_start_errors_are_visible() {
        for values in [
            vec!["github-adapter"],
            vec!["github-adapter", "start"],
            vec!["github-adapter", "start", "github"],
        ] {
            assert!(interactive_start(&arguments(&values), true));
        }
    }

    #[test]
    fn shell_and_redirected_start_errors_remain_nonmodal() {
        assert!(!interactive_start(&arguments(&["github-adapter"]), false));
        assert!(!interactive_start(
            &arguments(&["github-adapter", "start"]),
            false
        ));
    }

    #[test]
    fn foreground_and_internal_commands_never_show_startup_dialogs() {
        for command in [
            "auto",
            "github",
            "login",
            "doctor",
            "status",
            "open",
            "stop",
            "--help",
            "--version",
            "--background-host",
            "--host-job-probe",
            "--diagnose-host-jobs",
        ] {
            assert!(!interactive_start(
                &arguments(&["github-adapter", command]),
                true
            ));
        }
    }
}
