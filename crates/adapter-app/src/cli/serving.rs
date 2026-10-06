//! Foreground listener ownership, startup ordering, and bounded server shutdown.

use std::time::Duration;

use adapter_protocol::{AdapterError, Result};
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::Provider;
use adapter_runtime::server::ServerOptions;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::clients::prepare_frontend;
use super::discovery::{catalog_models, provider_name, select_backend};
use super::options::{Options, ProviderMode};
use super::readiness::{
    HEALTH_BYTES, MODEL_BYTES, READINESS_TIMEOUT, endpoint, local_error, local_json,
    matching_health,
};
use super::services::{Output, Services, cancelled};
use crate::listener;

struct Running {
    task: Option<JoinHandle<Result<()>>>,
    cancellation: CancellationToken,
    timeout: Duration,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn joined(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    result.map_err(|_| {
        AdapterError::new(
            500,
            "server_task_failed",
            "The foreground server task failed.",
        )
    })?
}

impl Running {
    async fn stop(&mut self) -> Result<()> {
        self.cancellation.cancel();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(self.timeout, &mut task).await {
                Ok(result) => joined(result),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    Err(AdapterError::new(
                        504,
                        "shutdown_deadline",
                        "The owned foreground server exceeded its drain deadline. Its tasks were cancelled.",
                    ))
                }
            }
        } else {
            Ok(())
        }
    }

    async fn wait(&mut self, cancellation: &CancellationToken) -> Result<()> {
        let result = {
            let task = self.task.as_mut().ok_or_else(|| {
                AdapterError::new(
                    500,
                    "server_task_failed",
                    "The foreground server task is unavailable.",
                )
            })?;
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => None,
                result = task => Some(result),
            }
        };
        match result {
            Some(result) => {
                self.task.take();
                self.cancellation.cancel();
                joined(result)
            }
            None => self.stop().await,
        }
    }
}

pub(super) async fn serve_command<S: Services>(
    services: &S,
    mut options: Options,
    cancellation: CancellationToken,
) -> Result<i32> {
    // The exclusive listener remains owned through discovery and configuration.
    // An occupied port never triggers a health-based reuse decision or a stop.
    let owned = listener::bind_loopback(options.address)?;
    let address = owned
        .local_addr()
        .map_err(|_| local_error("Cannot inspect the owned listener."))?;
    let listener = TcpListener::from_std(owned)
        .map_err(|_| local_error("Cannot initialize the owned listener."))?;
    services.bound(address)?;
    if options.choose_provider {
        options.provider = match services.choose_provider().await? {
            Provider::Mai => ProviderMode::Mai,
            Provider::Github => ProviderMode::Github,
        };
    }
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let selected = select_backend(services, &options, &cancellation).await?;
    let app = if let Some(launch) = options.launch {
        let context =
            RequestContext::with_cancellation(READINESS_TIMEOUT, cancellation.child_token())?;
        Some(
            context
                .bounded(services.apps(Some(launch.name().into())))
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    AdapterError::invalid(
                        "The requested desktop app is unavailable. Nothing was installed.",
                    )
                })?,
        )
    } else {
        None
    };
    let expected_model =
        prepare_frontend(services, &options, &selected, &endpoint(address)).await?;
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let server_options = ServerOptions::default();
    let server_cancel = cancellation.child_token();
    let mut running = Running {
        timeout: server_options.shutdown_timeout + Duration::from_secs(2),
        task: Some(services.serve(
            listener,
            selected.ready.backend.clone(),
            server_options,
            server_cancel.clone(),
        )),
        cancellation: server_cancel,
    };
    let startup = async {
        let context = RequestContext::with_cancellation(READINESS_TIMEOUT, cancellation.child_token())?;
        let health = local_json(address, "/health", HEALTH_BYTES, &context).await?;
        matching_health(health, &selected.ready.available.scope)?;
        if app.is_some() {
            let catalog = local_json(address, "/v1/models", MODEL_BYTES, &context).await?;
            let models = catalog_models(&catalog, &selected.ready.available.scope)?;
            if expected_model.as_ref().is_some_and(|id| !models.iter().any(|model| model["slug"].as_str() == Some(id))) {
                return Err(AdapterError::upstream("The local adapter no longer advertises the selected model. No fallback or app launch was attempted."));
            }
        }
        services.output(Output::Diagnostic, &services.redact(&selected.ready.backend, &format!(
            "Serving {} via {} in the foreground. Ctrl+C stops this owned server. No tray or instance reuse is active.",
            endpoint(address), provider_name(selected.ready.available.scope.provider),
        )))?;
        services.ready(address)?;
        if let Some(app) = app {
            if cancellation.is_cancelled() { return Err(cancelled()); }
            context.bounded(services.launch(app)).await?;
        }
        Ok(())
    }.await;
    if let Err(error) = startup {
        let _ = running.stop().await;
        return Err(error);
    }
    running.wait(&cancellation).await?;
    Ok(0)
}
