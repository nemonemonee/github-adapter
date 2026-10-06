use std::process::ExitCode;

use adapter_app::{cli, host, images, on_demand, startup_error};

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    let auxiliary = images::auxiliary(&arguments).or_else(|| on_demand::auxiliary(&arguments));
    let show_startup_error = auxiliary.is_none() && startup_error::enabled(&arguments);
    let result = auxiliary.unwrap_or_else(|| {
        host::entry_from(arguments).and_then(|entry| {
            if matches!(&entry, host::Entry::Foreground(_)) {
                let _ = tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::WARN)
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(std::io::stderr)
                    .try_init();
            }
            host::run_entry(entry)
        })
    });
    match result {
        Ok(status) => {
            if status != 0 && show_startup_error {
                startup_error::show();
            }
            ExitCode::from(u8::try_from(status).unwrap_or(1))
        }
        Err(error) => {
            eprintln!("{}", cli::error_text(&error));
            if show_startup_error {
                startup_error::show();
            }
            ExitCode::from(host::error_exit_code(&error))
        }
    }
}
