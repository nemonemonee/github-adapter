use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use adapter_protocol::Result;
use futures_util::future::BoxFuture;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    Activation, Control, Fault, Limits, PROTOCOL_VERSION, Request, Response, Snapshot, StartupSpec,
    State, failure,
};

/// A backend and activation seam; synthetic hosts need no credentials, settings, or UI.
pub trait Driver: Send + Sync + 'static {
    fn serve(
        &self,
        host: HostHandle,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<i32>>;
    fn open_codex(&self) -> BoxFuture<'_, Result<()>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Status,
    Open,
    Quit,
}

type Wake = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    snapshot: Snapshot,
    last_open: Option<Instant>,
    wake: Option<Wake>,
}

struct Shared {
    specification: StartupSpec,
    inner: Mutex<Inner>,
    shutdown: CancellationToken,
    opens: mpsc::Sender<u64>,
}

#[derive(Clone)]
pub struct HostHandle(Arc<Shared>);

impl HostHandle {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn create(
        specification: StartupSpec,
        shutdown: CancellationToken,
    ) -> (Self, mpsc::Receiver<u64>) {
        let (opens, receiver) = mpsc::channel(4);
        let snapshot = Snapshot {
            state: State::Starting,
            address: None,
            activation: if specification.opens_codex {
                Activation::Pending
            } else {
                Activation::NotRequested
            },
            activation_attempt: 0,
            diagnostics: Vec::new(),
        };
        (
            Self(Arc::new(Shared {
                specification,
                inner: Mutex::new(Inner {
                    snapshot,
                    last_open: None,
                    wake: None,
                }),
                shutdown,
                opens,
            })),
            receiver,
        )
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().snapshot.clone()
    }

    fn update(&self, apply: impl FnOnce(&mut Inner)) {
        let wake = {
            let mut state = self.lock();
            apply(&mut state);
            state.wake.clone()
        };
        if let Some(wake) = wake {
            wake();
        }
    }

    pub(crate) fn set_wake(&self, wake: Wake) {
        self.lock().wake = Some(wake);
    }

    pub fn backend_ready(&self, address: SocketAddr) {
        if !address.ip().is_loopback() {
            return;
        }
        self.update(|inner| {
            if inner.snapshot.state == State::Starting && !self.0.shutdown.is_cancelled() {
                inner.snapshot.state = State::Ready;
                inner.snapshot.address = Some(address);
            }
        });
    }

    /// Called by the serving CLI only after its independent readiness/configuration checks.
    pub fn initial_open(&self) -> Result<()> {
        self.queue_open(false, true)
            .map_err(|fault| failure(&fault.code, &fault.message))
    }

    fn queue_open(&self, automatic: bool, initial: bool) -> std::result::Result<(), Fault> {
        let wake = {
            let mut inner = self.lock();
            if inner.snapshot.state != State::Ready || self.0.shutdown.is_cancelled() {
                return Err(Fault::new(
                    "host_not_ready",
                    "The owned backend is not ready. No app was launched.",
                ));
            }
            match inner.snapshot.activation {
                Activation::Opening => return Ok(()),
                Activation::Pending if !initial => return Ok(()),
                Activation::Uncertain => {
                    return Err(Fault::new(
                        "activation_uncertain",
                        "A previous activation timed out and may finish late. No retry was issued. Inspect Codex before explicitly stopping/restarting the adapter.",
                    ));
                }
                Activation::Failed if automatic => {
                    return Err(Fault::new(
                        "activation_failed",
                        "The previous activation failed. Automatic start does not retry it. Inspect Codex and use explicit open if appropriate.",
                    ));
                }
                _ => {}
            }
            if automatic
                && inner
                    .last_open
                    .is_some_and(|last| last.elapsed() < Duration::from_secs(2))
            {
                return Ok(());
            }
            let attempt = inner.snapshot.activation_attempt + 1;
            self.0.opens.try_send(attempt).map_err(|_| {
                Fault::new(
                    "host_control_busy",
                    "The bounded host action queue is full. No app was launched.",
                )
            })?;
            inner.snapshot.activation_attempt = attempt;
            inner.snapshot.activation = Activation::Opening;
            inner.wake.clone()
        };
        if let Some(wake) = wake {
            wake();
        }
        Ok(())
    }

    pub fn route_tray(&self, action: TrayAction) -> Response {
        match action {
            TrayAction::Status => self.response(None),
            TrayAction::Open => self.response(self.queue_open(false, false).err()),
            TrayAction::Quit => {
                self.stop();
                self.response(None)
            }
        }
    }

    pub fn request(&self, request: &Request) -> Response {
        if let Err(error) = request.validate() {
            return self.response(Some(Fault::new(&error.code, &error.message)));
        }
        let error = match &request.command {
            Control::Status => None,
            Control::Stop => {
                self.stop();
                None
            }
            Control::Start {
                specification,
                credentials,
            } => {
                if specification != &self.0.specification.specification
                    || credentials != &self.0.specification.credentials
                {
                    Some(Fault::new(
                        "host_configuration_mismatch",
                        "The running host has different startup options, account selection, or credentials. Nothing was changed or launched. Stop it explicitly before starting the new configuration.",
                    ))
                } else if self.snapshot().state == State::Ready && self.0.specification.opens_codex
                {
                    self.queue_open(true, false).err()
                } else {
                    None
                }
            }
            Control::Open { credentials } => {
                if credentials != &self.0.specification.credentials {
                    Some(Fault::new(
                        "restart_required",
                        "Credentials or token overrides changed. Stop and start the host. No account was switched and no app was launched.",
                    ))
                } else {
                    self.queue_open(false, false).err()
                }
            }
        };
        self.response(error)
    }

    pub(crate) fn response(&self, error: Option<Fault>) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            process_id: std::process::id(),
            snapshot: self.snapshot(),
            error,
            exited: false,
        }
    }

    pub fn stop(&self) {
        self.update(|inner| inner.snapshot.state = State::Stopping);
        self.0.shutdown.cancel();
    }

    pub(crate) fn fail(&self, fault: Fault) {
        self.update(|inner| {
            inner.snapshot.state = State::Failed;
            if inner.snapshot.activation == Activation::Pending {
                inner.snapshot.activation = Activation::NotRequested;
            }
            remember(&mut inner.snapshot, fault);
        });
    }

    #[cfg(windows)]
    pub(crate) fn ui_failure(&self) {
        self.fail(Fault::new("tray_unavailable",
            "The native tray could not be maintained. The owned backend is shutting down; no invisible background host is retained."));
        self.0.shutdown.cancel();
    }

    fn activation_done(&self, attempt: u64, result: Result<()>) {
        self.update(|inner| {
            if inner.snapshot.activation_attempt != attempt {
                return;
            }
            inner.last_open = Some(Instant::now());
            inner.snapshot.activation = match result {
                Ok(()) => Activation::Acknowledged,
                Err(error) => {
                    let fault = Fault::from_error(&error, true);
                    let uncertain = fault.code == "activation_uncertain";
                    remember(&mut inner.snapshot, fault);
                    if uncertain {
                        Activation::Uncertain
                    } else {
                        Activation::Failed
                    }
                }
            };
        });
    }
}

fn remember(snapshot: &mut Snapshot, fault: Fault) {
    if snapshot.diagnostics.len() == 8 {
        snapshot.diagnostics.remove(0);
    }
    snapshot.diagnostics.push(fault);
}

struct Task<T>(Option<JoinHandle<T>>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

pub(crate) async fn supervise(
    host: HostHandle,
    mut opens: mpsc::Receiver<u64>,
    driver: Arc<dyn Driver>,
    limits: Limits,
) -> Result<()> {
    let backend_cancel = host.0.shutdown.child_token();
    let cancel_on_drop = backend_cancel.clone().drop_guard();
    let backend_host = host.clone();
    let backend_driver = driver.clone();
    let cancellation = backend_cancel.clone();
    let mut backend = Task(Some(tokio::spawn(async move {
        backend_driver.serve(backend_host, cancellation).await
    })));
    let mut opening: Task<(u64, Result<()>)> = Task(None);
    let startup = tokio::time::sleep(limits.startup);
    tokio::pin!(startup);
    let mut startup_expired = false;
    loop {
        tokio::select! {
            biased;
            _ = host.0.shutdown.cancelled() => break,
            result = async { backend.0.as_mut().expect("guarded").await }, if backend.0.is_some() => {
                backend.0.take();
                let error = match result {
                    Ok(Err(error)) => error,
                    _ => failure("server_exited", "The owned backend ended unexpectedly."),
                };
                if host.snapshot().state != State::Failed {
                    host.fail(Fault::from_error(&error, false));
                }
            }
            _ = &mut startup, if !startup_expired => {
                startup_expired = true;
                if host.snapshot().state == State::Starting {
                    backend_cancel.cancel();
                    host.fail(Fault::from_error(&failure("startup_deadline", "Startup expired."), false));
                }
            }
            Some(attempt) = opens.recv(), if opening.0.is_none() => {
                if host.snapshot().state == State::Ready {
                    let driver = driver.clone();
                    opening.0 = Some(tokio::spawn(async move {
                        let result = tokio::time::timeout(limits.activation, driver.open_codex()).await
                            .unwrap_or_else(|_| Err(failure("activation_timeout", "Activation acknowledgment timed out.")));
                        (attempt, result)
                    }));
                }
            }
            result = async { opening.0.as_mut().expect("guarded").await }, if opening.0.is_some() => {
                opening.0.take();
                match result {
                    Ok((attempt, result)) => host.activation_done(attempt, result),
                    Err(_) => host.activation_done(host.snapshot().activation_attempt,
                        Err(failure("activation_timeout", "The activation task ended without an acknowledgment."))),
                }
            }
        }
    }
    host.stop();
    backend_cancel.cancel();
    if let Some(task) = opening.0.take() {
        task.abort();
        let _ = task.await;
        host.activation_done(
            host.snapshot().activation_attempt,
            Err(failure(
                "activation_timeout",
                "Shutdown interrupted activation acknowledgment.",
            )),
        );
    }
    let result = if let Some(mut task) = backend.0.take() {
        match tokio::time::timeout(limits.drain, &mut task).await {
            Ok(Ok(Ok(_))) => Ok(()),
            Ok(Ok(Err(error))) if error.status == 499 => Ok(()),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err(failure(
                "server_task_failed",
                "The owned backend task ended during shutdown.",
            )),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(failure(
                    "shutdown_deadline",
                    "The owned server exceeded its drain deadline.",
                ))
            }
        }
    } else {
        Ok(())
    };
    drop(cancel_on_drop);
    if let Err(error) = &result {
        host.update(|inner| remember(&mut inner.snapshot, Fault::from_error(error, false)));
    }
    result
}
