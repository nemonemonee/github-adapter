//! Optional, native-only image commands and standard stdio MCP. No listener or autostart.
mod mcp;
use crate::{
    cli,
    config::{self, Client, ClientPaths, images as registration},
};
use adapter_protocol::{AdapterError, Result, Value};
use adapter_runtime::{
    context::RequestContext,
    images::{self, ImageConfig, ImageProtocol},
};
use clap::{Parser, ValueEnum, error::ErrorKind};
use serde_json::json;
use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Action {
    #[value(name = "image-enable")]
    Enable,
    #[value(name = "image-disable")]
    Disable,
    #[value(name = "image-status")]
    Status,
    #[value(name = "image-generate")]
    Generate,
    #[value(name = "image-mcp")]
    Mcp,
}
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Protocol {
    OpenaiImages,
    QwenMessages,
}
#[derive(Parser)]
#[command(
    name = "github-adapter",
    about = "Optional image provider: explicit configuration, one generation per request, no SDK or coding-provider changes."
)]
struct Arguments {
    #[arg(value_enum)]
    action: Action,
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_enum)]
    protocol: Option<Protocol>,
    #[arg(
        long,
        help = "Read the image API key from this explicitly named environment variable; never put a key in argv. Otherwise prompt with echo disabled."
    )]
    key_env: Option<String>,
    #[arg(long)]
    codex_config: Option<PathBuf>,
    #[arg(long)]
    backup_dir: Option<PathBuf>,
    #[arg(
        long,
        help = "Check local MCP initialize/tools/list without generating an image."
    )]
    check: bool,
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(
        long,
        default_value = "auto",
        help = "auto chooses the configured model's square default; image-status lists supported sizes."
    )]
    size: String,
}
fn error(message: &str) -> AdapterError {
    AdapterError::invalid(message)
}
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| error("Could not start the native image runtime."))
}
fn program() -> Result<PathBuf> {
    let exe =
        std::env::current_exe().map_err(|_| error("The native executable path is unavailable."))?;
    if exe
        .file_name()
        .is_some_and(|n| n == "github-adapter-host.exe" || n == "github-adapter-host")
    {
        Ok(exe.with_file_name(if cfg!(windows) {
            "github-adapter.exe"
        } else {
            "github-adapter"
        }))
    } else {
        Ok(exe)
    }
}
fn print(value: &Value) -> Result<()> {
    serde_json::to_writer(std::io::stdout().lock(), value)
        .map_err(|_| error("Could not write image command output."))?;
    println!();
    Ok(())
}
pub fn auxiliary(args: &[OsString]) -> Option<Result<i32>> {
    if !args
        .get(1)
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("image-"))
    {
        return None;
    }
    Some(run(args))
}
fn run(args: &[OsString]) -> Result<i32> {
    let options = match Arguments::try_parse_from(args) {
        Ok(options) => options,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            print!("{e}");
            return Ok(0);
        }
        Err(e) => return Err(error(&e.to_string())),
    };
    let provider_options = options.endpoint.is_some()
        || options.model.is_some()
        || options.key_env.is_some()
        || options.protocol.is_some();
    if (provider_options && !matches!(options.action, Action::Enable))
        || (options.check && !matches!(options.action, Action::Status))
        || ((options.prompt_file.is_some() || options.output.is_some())
            && !matches!(options.action, Action::Generate))
    {
        return Err(error(
            "These image options do not apply to the selected command.",
        ));
    }
    let paths = config::default_paths(
        options.codex_config,
        None,
        options.backup_dir,
        &[Client::Codex],
    )?;
    let exe = program()?;
    match options.action {
        Action::Enable => {
            let state = if !provider_options {
                registration::repair(&paths, &exe)?
            } else {
                let endpoint=options.endpoint.ok_or_else(||error("image-enable requires --endpoint and --model, or no provider options to repair existing registration."))?;
                let model = options
                    .model
                    .ok_or_else(|| error("image-enable requires --model."))?;
                let provider = ImageConfig {
                    endpoint,
                    model,
                    protocol: match options.protocol.unwrap_or(Protocol::OpenaiImages) {
                        Protocol::OpenaiImages => ImageProtocol::OpenaiImages,
                        Protocol::QwenMessages => ImageProtocol::QwenMessages,
                    },
                };
                provider.validate()?;
                let key=match options.key_env {
                    Some(name)=>std::env::var(name).map_err(|_|error("The explicitly named image-key environment variable is missing or not UTF-8."))?,
                    None=>cli::read_hidden_secret("Image provider API key (input hidden): ")?,
                };
                registration::enable(&paths, &exe, provider, &key)?
            };
            print(&state)?;
        }
        Action::Disable => print(&registration::disable(&paths)?)?,
        Action::Status => {
            let mut state = registration::status(&paths)?;
            if options.check {
                state["mcp_probe"] = probe(&paths, &exe)?;
            }
            print(&state)?;
        }
        Action::Generate => {
            let input = options.prompt_file.ok_or_else(|| {
                error("image-generate requires --prompt-file; do not put private prompts in argv.")
            })?;
            let output = options
                .output
                .ok_or_else(|| error("image-generate requires --output for a new image file."))?;
            let mut file = std::fs::File::open(input)
                .map_err(|_| error("The prompt file could not be opened."))?;
            let mut prompt = String::new();
            Read::take(&mut file, 64 * 1024 + 1)
                .read_to_string(&mut prompt)
                .map_err(|_| error("The prompt file must contain UTF-8 text."))?;
            // One resolved reservation rejects existing files before any billable request.
            let reserved = registration::reserve_output(&output)?;
            let ctx = RequestContext::new(Duration::from_secs(240))?;
            let image = generate(&paths, &prompt, &options.size, ctx)?;
            let output = reserved.write(&image.bytes)?;
            print(
                &json!({"output":output,"mime_type":image.mime_type,"width":image.width,"height":image.height,"generation_attempts":1}),
            )?;
        }
        Action::Mcp => {
            registration::startup(&paths, &exe)?;
            runtime()?.block_on(mcp::serve(paths))?;
        }
    }
    Ok(0)
}
fn generate(
    paths: &ClientPaths,
    prompt: &str,
    size: &str,
    ctx: RequestContext,
) -> Result<images::GeneratedImage> {
    let _generation = registration::generation_lock(paths)?;
    let (provider, key) = registration::snapshot(paths)?;
    runtime()?.block_on(images::generate(&provider, &key, prompt, size, ctx))
}

const PROBE_BYTES: usize = 512 * 1024;

async fn probe_output(command: &mut tokio::process::Command, budget: Duration) -> Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW, including failed readiness checks.
    let mut child = command
        .spawn()
        .map_err(|_| error("The native image MCP helper could not start."))?;
    // One deadline covers stdin, bounded stdout AND process exit. Dropping the
    // owned child on timeout/output failure kills it; no reader thread can hang.
    tokio::time::timeout(budget, async {
        let request = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"github-adapter-status","version":"1"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
        );
        let mut input = child.stdin.take()
            .ok_or_else(|| error("MCP stdin is unavailable."))?;
        input.write_all(request.as_bytes()).await
            .map_err(|_| error("MCP initialization could not be written."))?;
        drop(input); // EOF lets a successful readiness-only helper exit normally.
        let output = child.stdout.take()
            .ok_or_else(|| error("MCP stdout is unavailable."))?;
        let mut bytes = Vec::new();
        output.take(PROBE_BYTES as u64 + 1).read_to_end(&mut bytes).await
            .map_err(|_| error("MCP readiness output could not be read."))?;
        if bytes.len() > PROBE_BYTES {
            return Err(error("MCP readiness output exceeds its byte limit."));
        }
        let exit = child.wait().await
            .map_err(|_| error("MCP readiness process did not exit."))?;
        if !exit.success() {
            return Err(error("MCP readiness returned an invalid response."));
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| error("MCP readiness check timed out; no image was generated."))?
}

fn probe(paths: &ClientPaths, exe: &Path) -> Result<Value> {
    let mut command = tokio::process::Command::new(exe);
    command.arg("image-mcp");
    if let Some(codex) = &paths.codex_config {
        command.arg("--codex-config").arg(codex);
    }
    command.arg("--backup-dir").arg(&paths.backup_dir);
    let bytes = runtime()?.block_on(probe_output(&mut command, Duration::from_secs(10)))?;
    let frames: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line).map_err(|_| error("MCP readiness output was not JSON."))
        })
        .collect::<Result<_>>()?;
    let initialized = frames.iter().any(|frame| {
        frame["id"] == 1 && frame["result"]["serverInfo"]["name"] == "github-adapter-image"
    });
    let available = frames
        .iter()
        .find(|frame| frame["id"] == 2)
        .and_then(|frame| frame["result"]["tools"].as_array())
        .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "generate_image"));
    Ok(json!({"reachable":initialized,"image_tool_available":available,"generation_attempts":0}))
}

pub fn startup(paths: &ClientPaths) -> Result<()> {
    registration::startup(paths, &program()?)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::Write;

    fn child(mode: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "images::tests::probe_child_fixture",
            "--ignored",
            "--nocapture",
        ]);
        command.env("GITHUB_ADAPTER_TEST_PROBE_MODE", mode);
        command
    }

    #[test]
    #[ignore = "Launched explicitly by bounded readiness tests; never a standalone qualification."]
    fn probe_child_fixture() {
        let mode = std::env::var("GITHUB_ADAPTER_TEST_PROBE_MODE").unwrap();
        let mut request = String::new();
        std::io::stdin().read_to_string(&mut request).unwrap();
        assert!(request.contains("tools/list"));
        assert!(!request.contains("tools/call"));
        match mode.as_str() {
            "ok" => {
                print!("probe-ready");
                std::io::stdout().flush().unwrap();
                std::process::exit(0);
            }
            "failure" => std::process::exit(7),
            "eof-without-exit" => {
                use windows_sys::Win32::{
                    Foundation::CloseHandle,
                    System::Console::{GetStdHandle, STD_OUTPUT_HANDLE},
                };
                std::io::stdout().flush().unwrap();
                // Close only this explicitly spawned test child's stdout, not the parent.
                assert_ne!(unsafe { CloseHandle(GetStdHandle(STD_OUTPUT_HANDLE)) }, 0);
            }
            "oversized-without-exit" => {
                let _ = std::io::stdout().write_all(&vec![b'x'; PROBE_BYTES + 1]);
                let _ = std::io::stdout().flush();
            }
            _ => panic!("Unknown readiness fixture mode"),
        }
        std::thread::sleep(Duration::from_secs(60));
    }

    #[tokio::test]
    async fn readiness_requires_successful_helper_exit_without_generating() {
        let bytes = probe_output(&mut child("ok"), Duration::from_secs(5))
            .await
            .unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("probe-ready"));
        let failure = probe_output(&mut child("failure"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(failure.message.contains("invalid response"));
    }

    #[tokio::test]
    async fn readiness_deadline_also_covers_a_helper_that_closes_stdout_without_exiting() {
        let failure = tokio::time::timeout(
            Duration::from_secs(5),
            probe_output(&mut child("eof-without-exit"), Duration::from_secs(2)),
        )
        .await
        .expect("Readiness must not block waiting for child exit")
        .unwrap_err();
        assert!(failure.message.contains("timed out"));
    }

    #[tokio::test]
    async fn oversized_readiness_output_never_waits_for_a_still_running_helper() {
        let failure = tokio::time::timeout(
            Duration::from_secs(5),
            probe_output(&mut child("oversized-without-exit"), Duration::from_secs(4)),
        )
        .await
        .expect("Oversized output must not leave an unbounded child wait")
        .unwrap_err();
        assert!(failure.message.contains("byte limit"));
    }
}
