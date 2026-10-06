use adapter_protocol::{AdapterError, Result};
use adapter_runtime::backend::{Backend, BackendConfig};
use adapter_runtime::context::RequestContext;
use adapter_runtime::server::{ServerOptions, serve};
use serde_json::json;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Synthetic relay fixture: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.len() != 2 {
        return Err(AdapterError::invalid(
            "Pass a synthetic loopback upstream and its model-cache path.",
        ));
    }
    let upstream =
        Url::parse(&arguments[0]).map_err(|_| AdapterError::invalid("Invalid fixture URL."))?;
    if upstream.scheme() != "http"
        || !matches!(upstream.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
        || !upstream.username().is_empty()
        || upstream.password().is_some()
    {
        return Err(AdapterError::invalid(
            "The benchmark accepts only a literal HTTP loopback fixture.",
        ));
    }
    let backend = Backend::connect(
        BackendConfig::Mai {
            upstream: arguments[0].clone(),
            models_cache: PathBuf::from(&arguments[1]),
        },
        RequestContext::new(Duration::from_secs(5))?,
    )
    .await?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| AdapterError::new(500, "fixture_bind", "Cannot bind the fixture listener."))?;
    println!(
        "{}",
        json!({
            "port": listener.local_addr().map_err(|_| AdapterError::invalid("Invalid fixture listener."))?.port(),
            "max_active_requests": 32,
        })
    );
    std::io::stdout()
        .flush()
        .map_err(|_| AdapterError::invalid("Cannot announce the fixture port."))?;
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    std::thread::spawn(move || {
        let mut input = std::io::stdin();
        let mut bytes = [0; 1];
        if let Err(error) = input.read(&mut bytes) {
            eprintln!("Synthetic fixture control input failed: {error}");
        }
        stop.cancel();
    });
    serve(
        listener,
        backend,
        ServerOptions {
            max_active_requests: 32,
            ..ServerOptions::default()
        },
        shutdown,
    )
    .await
}
