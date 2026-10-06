#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use adapter_app::{cli, host, on_demand, startup_error};

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    let auxiliary = on_demand::auxiliary(&arguments);
    let interactive = auxiliary.is_none();
    match auxiliary.unwrap_or_else(|| host::run_desktop_from(arguments)) {
        Ok(status) => {
            if status != 0 && interactive {
                startup_error::show();
            }
            ExitCode::from(u8::try_from(status).unwrap_or(1))
        }
        Err(error) => {
            eprintln!("{}", cli::error_text(&error));
            if interactive {
                startup_error::show();
            }
            ExitCode::from(host::error_exit_code(&error))
        }
    }
}
