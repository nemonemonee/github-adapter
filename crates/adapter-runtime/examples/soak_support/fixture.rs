use adapter_protocol::sse::{SseDecoder, SseEvent, StreamLimits};
use adapter_runtime::backend::{Backend, BackendConfig};
use adapter_runtime::context::RequestContext;
use adapter_runtime::server::{ServerOptions, serve};
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::report::{Config, Drain, Evidence, millis};
use crate::{Check, SharedStatistics, require, support};

pub const REQUEST_BYTES: usize = 16 * 1024;
pub const RESPONSE_BYTES: usize = 64 * 1024;
const DELTAS: usize = 16;
const MALFORMED_SSE: &[u8] = b"data: {not-json}\n\n";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Json,
    Sse,
    Slow,
    Hold,
    Unavailable,
    Malformed,
}

struct Ticket {
    mode: Mode,
    calls: AtomicU64,
    released: AtomicU64,
    completed: AtomicU64,
    gate: CancellationToken,
}

struct Registration {
    id: String,
    ticket: Arc<Ticket>,
    state: Arc<Upstream>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.state.tickets.lock().unwrap().remove(&self.id);
    }
}

impl Registration {
    async fn verify(&self, expected_calls: u64, expected_completion: bool) -> Check<()> {
        if expected_calls > 0 {
            tokio::time::timeout(REQUEST_TIMEOUT, async {
                while self.ticket.released.load(Ordering::SeqCst) < expected_calls {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|_| "Upstream body did not release within 3 s.")?;
        }
        require(
            self.ticket.calls.load(Ordering::SeqCst) == expected_calls,
            "Upstream call count changed: replay, retry, or missing request",
        )?;
        require(
            self.ticket.completed.load(Ordering::SeqCst)
                == if expected_completion {
                    expected_calls
                } else {
                    0
                },
            "Unexpected upstream completion/cancellation outcome",
        )
    }
}

struct Upstream {
    tickets: Mutex<HashMap<String, Arc<Ticket>>>,
    admission: Arc<Semaphore>,
    posts: AtomicU64,
    metadata_gets: AtomicU64,
    unexpected: AtomicU64,
    duplicates: AtomicU64,
    active: AtomicU64,
    peak_active: AtomicU64,
    completed: AtomicU64,
    early_drops: AtomicU64,
}

struct Lease {
    state: Arc<Upstream>,
    ticket: Arc<Ticket>,
    finished: bool,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        if self.finished {
            self.ticket.completed.fetch_add(1, Ordering::SeqCst);
            self.state.completed.fetch_add(1, Ordering::SeqCst);
        } else {
            self.state.early_drops.fetch_add(1, Ordering::SeqCst);
        }
        self.ticket.released.fetch_add(1, Ordering::SeqCst);
    }
}

struct OwnedTask(JoinHandle<Check<()>>);

impl OwnedTask {
    async fn join(&mut self) -> Check<()> {
        match tokio::time::timeout(JOIN_TIMEOUT, &mut self.0).await {
            Ok(result) => result.map_err(|e| format!("Owned server task failed: {e}"))?,
            Err(_) => {
                self.0.abort();
                let _ = (&mut self.0).await;
                Err("Owned server exceeded its drain deadline; aborted, not clean.".into())
            }
        }
    }
}

impl Drop for OwnedTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct Fixture {
    backend: Arc<Backend>,
    endpoint: String,
    origin: String,
    client: reqwest::Client,
    state: Arc<Upstream>,
    statistics: SharedStatistics,
    shutdown: CancellationToken,
    origin_shutdown: CancellationToken,
    runtime: tokio::sync::Mutex<OwnedTask>,
    upstream: tokio::sync::Mutex<OwnedTask>,
    cache: PathBuf,
    concurrency: usize,
}

impl Fixture {
    pub async fn start(
        config: &Config,
        directory: &std::path::Path,
        run_id: &str,
        statistics: SharedStatistics,
    ) -> Check<Arc<Self>> {
        let state = Arc::new(Upstream {
            tickets: Mutex::new(HashMap::new()),
            admission: Arc::new(Semaphore::new(config.concurrency + 1)),
            posts: AtomicU64::new(0),
            metadata_gets: AtomicU64::new(0),
            unexpected: AtomicU64::new(0),
            duplicates: AtomicU64::new(0),
            active: AtomicU64::new(0),
            peak_active: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            early_drops: AtomicU64::new(0),
        });
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| e.to_string())?;
        let origin = format!(
            "http://{}",
            listener.local_addr().map_err(|e| e.to_string())?
        );
        let stopping = CancellationToken::new();
        let stop = stopping.clone();
        let router = Router::new()
            .route("/", get(origin_health))
            .route("/v1/models", get(origin_models))
            .route("/v1/responses", post(origin_inference))
            .fallback(origin_unknown)
            .with_state(state.clone());
        // Axum owns HTTP framing; only the fixture's bounded, synthetic payloads are implemented here.
        let upstream = OwnedTask(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .map_err(|e| e.to_string())
        }));
        let cache = directory.join(format!("native-soak-models-{run_id}.json"));
        let cache_value = json!({"models": [{
            "slug": "fixture-model", "display_name": "Synthetic soak model",
            "context_window": 32000, "supported_reasoning_levels": [],
        }]});
        std::fs::write(
            &cache,
            serde_json::to_vec(&cache_value).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let result = Self::connect(
            config,
            origin,
            stopping,
            upstream,
            cache.clone(),
            state,
            statistics,
        )
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(cache);
        }
        result
    }

    async fn connect(
        config: &Config,
        origin: String,
        origin_shutdown: CancellationToken,
        upstream: OwnedTask,
        cache: PathBuf,
        state: Arc<Upstream>,
        statistics: SharedStatistics,
    ) -> Check<Arc<Self>> {
        let backend = Backend::connect(
            BackendConfig::Mai {
                upstream: origin.clone(),
                models_cache: cache.clone(),
            },
            RequestContext::new(REQUEST_TIMEOUT).map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| e.to_string())?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| e.to_string())?;
        let endpoint = format!(
            "http://{}",
            listener.local_addr().map_err(|e| e.to_string())?
        );
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let server_backend = backend.clone();
        let options = ServerOptions {
            max_active_requests: config.concurrency,
            max_request_bytes: REQUEST_BYTES,
            body_timeout: REQUEST_TIMEOUT,
            inference_timeout: REQUEST_TIMEOUT,
            header_timeout: REQUEST_TIMEOUT,
            shutdown_timeout: Duration::from_secs(3),
        };
        let runtime = OwnedTask(tokio::spawn(async move {
            serve(listener, server_backend, options, stop)
                .await
                .map_err(|e| e.to_string())
        }));
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http1_only()
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(config.concurrency + 2)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            backend,
            endpoint,
            origin,
            client,
            state,
            statistics,
            shutdown,
            origin_shutdown,
            runtime: tokio::sync::Mutex::new(runtime),
            upstream: tokio::sync::Mutex::new(upstream),
            cache,
            concurrency: config.concurrency,
        }))
    }

    fn register(&self, id: &str, mode: Mode) -> Check<Registration> {
        let ticket = Arc::new(Ticket {
            mode,
            calls: AtomicU64::new(0),
            released: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            gate: CancellationToken::new(),
        });
        let mut tickets = self.state.tickets.lock().unwrap();
        require(
            tickets.len() < self.concurrency + 2 && !tickets.contains_key(id),
            "Fixture ticket bound or unique ID invariant failed",
        )?;
        tickets.insert(id.into(), ticket.clone());
        Ok(Registration {
            id: id.into(),
            ticket,
            state: self.state.clone(),
        })
    }

    fn request(id: &str, stream: bool) -> Value {
        json!({"model": "fixture-model", "input": id, "stream": stream, "store": false})
    }

    async fn post(&self, payload: &Value) -> Check<reqwest::Response> {
        self.post_raw(serde_json::to_vec(payload).map_err(|e| e.to_string())?)
            .await
    }

    async fn post_raw(&self, bytes: Vec<u8>) -> Check<reqwest::Response> {
        self.statistics.lock().unwrap().http_requests_started += 1;
        self.client
            .post(format!("{}/responses", self.endpoint))
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("HTTP POST: {e}"))
    }

    pub async fn health(&self) -> Check<Evidence> {
        self.statistics.lock().unwrap().http_requests_started += 1;
        let response = self
            .client
            .get(format!("{}/health", self.endpoint))
            .send()
            .await
            .map_err(|e| format!("Health: {e}"))?;
        require(response.status() == StatusCode::OK, "Health HTTP status")?;
        let value = read_json(response).await?;
        require(
            value["status"] == "ok"
                && value["engine"] == "rust"
                && value["application"] == "github-adapter"
                && value["provider"] == "mai"
                && value["account"].is_null()
                && value["upstream"] == self.origin,
            "Health shape, engine, account, or isolated upstream changed",
        )?;
        Ok(Evidence::default())
    }

    pub async fn admission(&self) -> Check<Evidence> {
        let mut held = Vec::new();
        for index in 0..self.concurrency {
            let registration = self.register(&format!("admission-{index}"), Mode::Hold)?;
            let response = self.held(&registration).await?;
            held.push((registration, response));
        }
        let denied = self.register("admission-denied", Mode::Json)?;
        expect_error(
            self.post(&Self::request(&denied.id, false)).await?,
            429,
            "rate_limit_error",
        )
        .await?;
        denied.verify(0, false).await?;
        self.health().await?;
        for (registration, response) in held {
            drop(response);
            registration.verify(1, false).await?;
        }
        Ok(Evidence {
            upstream_posts: self.concurrency as u64,
            cancellations: self.concurrency as u64,
            rejections: 1,
            ..Evidence::default()
        })
    }

    async fn held(&self, registration: &Registration) -> Check<reqwest::Response> {
        let mut response = self.post(&Self::request(&registration.id, true)).await?;
        require(
            response.status() == StatusCode::OK,
            "Held stream HTTP status",
        )?;
        let events = first_event(&mut response).await?;
        require(
            events.len() == 1
                && events[0].kind == "response.created"
                && events[0].data.as_ref().unwrap()["response"]["id"]
                    == response_id(&registration.id),
            "Held stream did not produce exactly its identified creation event",
        )?;
        Ok(response)
    }

    pub async fn case(&self, name: &str, id: &str) -> Check<Evidence> {
        let mode = match name {
            "sse" => Mode::Sse,
            "slow_sse" => Mode::Slow,
            "disconnect" | "cancel" => Mode::Hold,
            "upstream_error" => Mode::Unavailable,
            "malformed_sse" => Mode::Malformed,
            _ => Mode::Json,
        };
        let registration = self.register(id, mode)?;
        let mut evidence = Evidence {
            upstream_posts: 1,
            ..Evidence::default()
        };
        match name {
            "json" => {
                let response = self.post(&Self::request(id, false)).await?;
                require(response.status() == StatusCode::OK, "JSON HTTP status")?;
                validate_resource(&read_json(response).await?, id)?;
            }
            "sse" | "slow_sse" => {
                let response = self.post(&Self::request(id, true)).await?;
                require(response.status() == StatusCode::OK, "SSE HTTP status")?;
                require(
                    response
                        .headers()
                        .get("content-type")
                        .is_some_and(|v| v == "text/event-stream"),
                    "SSE content type",
                )?;
                let events = read_events(response, name == "slow_sse").await?;
                validate_events(&events, id)?;
                evidence.frames = events.len() as u64;
            }
            "disconnect" => {
                let response = self.held(&registration).await?;
                drop(response);
                evidence.cancellations = 1;
            }
            "cancel" => {
                self.statistics
                    .lock()
                    .unwrap()
                    .direct_backend_requests_started += 1;
                let context = RequestContext::new(REQUEST_TIMEOUT).map_err(|e| e.to_string())?;
                let mut stream = self
                    .backend
                    .clone()
                    .stream_responses(Self::request(id, true), context.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                let first = stream
                    .next()
                    .await
                    .ok_or("Cancellation stream had no first event")?
                    .map_err(|e| e.to_string())?;
                require(
                    first.kind == "response.created"
                        && first.data.as_ref().unwrap()["response"]["id"] == response_id(id),
                    "Cancellation stream identity",
                )?;
                context.cancellation.cancel();
                match stream.next().await {
                    Some(Err(error)) if error.code == "cancelled" && error.status == 499 => {}
                    _ => return Err("Controlled RequestContext cancellation did not return exact 499/cancelled.".into()),
                }
                drop(stream);
                evidence.cancellations = 1;
            }
            "malformed_json" | "malformed_shape" | "oversized_body" => {
                let (bytes, status, code) = match name {
                    "malformed_json" => (b"{not-json".to_vec(), 400, "invalid_request_error"),
                    "malformed_shape" => (b"[]".to_vec(), 400, "invalid_request_error"),
                    _ => {
                        let mut request = Self::request(id, false);
                        request["padding"] = json!("x".repeat(REQUEST_BYTES));
                        (
                            serde_json::to_vec(&request).map_err(|e| e.to_string())?,
                            413,
                            "request_too_large",
                        )
                    }
                };
                expect_error(self.post_raw(bytes).await?, status, code).await?;
                evidence.upstream_posts = 0;
                evidence.rejections = 1;
            }
            "upstream_error" => {
                let error = expect_error(
                    self.post(&Self::request(id, false)).await?,
                    503,
                    "upstream_error",
                )
                .await?;
                require(
                    error["error"]["message"].as_str().is_some_and(|message| {
                        message.contains("synthetic unavailable; never retry")
                    }),
                    "503 error did not come from the controlled synthetic rejection",
                )?;
                evidence.rejections = 1;
            }
            "malformed_sse" => {
                let response = self.held(&registration).await?;
                registration.ticket.gate.cancel();
                let events = read_events(response, false).await?;
                let expected = decoder()
                    .push(MALFORMED_SSE)
                    .err()
                    .ok_or("The malformed SSE fixture was unexpectedly accepted")?;
                require(
                    events.len() == 1
                        && events[0].kind == "error"
                        && events[0].data.as_ref().unwrap()["code"] == expected.code
                        && events[0].data.as_ref().unwrap()["message"] == expected.message,
                    "Malformed upstream SSE did not produce exactly one error without a completion",
                )?;
                evidence.rejections = 1;
                evidence.cancellations = 1;
                evidence.frames = 2;
            }
            _ => return Err("Unknown workload case.".into()),
        }
        registration
            .verify(evidence.upstream_posts, evidence.cancellations == 0)
            .await?;
        Ok(evidence)
    }

    pub fn snapshot(&self) -> Value {
        json!({
            "endpoint": self.origin,
            "runtime_endpoint": self.endpoint,
            "posts": self.state.posts.load(Ordering::SeqCst),
            "metadata_gets": self.state.metadata_gets.load(Ordering::SeqCst),
            "unexpected_requests": self.state.unexpected.load(Ordering::SeqCst),
            "duplicate_requests": self.state.duplicates.load(Ordering::SeqCst),
            "active_bodies": self.state.active.load(Ordering::SeqCst),
            "peak_active_bodies": self.state.peak_active.load(Ordering::SeqCst),
            "completed_bodies": self.state.completed.load(Ordering::SeqCst),
            "early_body_drops": self.state.early_drops.load(Ordering::SeqCst),
            "live_tickets": self.state.tickets.lock().unwrap().len(),
            "request_byte_limit": REQUEST_BYTES,
            "consumer_byte_limit": RESPONSE_BYTES,
            "history": self.backend.history().statistics().ok(),
        })
    }

    pub async fn stop(&self, workers_joined: bool) -> (Drain, Check<Evidence>) {
        let started = Instant::now();
        let mut drain = Drain {
            workers_joined,
            ..Drain::default()
        };
        let active_cancel = async {
            let registration = self.register("drain", Mode::Hold)?;
            let response = self.held(&registration).await?;
            self.shutdown.cancel();
            let events = read_events(response, false).await?;
            require(
                events.len() == 1
                    && events[0].kind == "error"
                    && events[0].data.as_ref().unwrap()["code"] == "cancelled",
                "Graceful shutdown did not cancel the active HTTP stream explicitly",
            )?;
            registration.verify(1, false).await?;
            Ok::<_, String>(())
        }
        .await;
        drain.active_request_cancelled = active_cancel.is_ok();
        self.shutdown.cancel();
        let runtime = self.runtime.lock().await.join().await;
        drain.runtime_joined = runtime.is_ok();
        self.origin_shutdown.cancel();
        let upstream = self.upstream.lock().await.join().await;
        drain.upstream_joined = upstream.is_ok();
        let rebound = async {
            for endpoint in [&self.endpoint, &self.origin] {
                let listener = TcpListener::bind(endpoint.strip_prefix("http://").unwrap())
                    .await
                    .map_err(|e| format!("Owned listener not released: {e}"))?;
                drop(listener);
            }
            Ok::<_, String>(())
        }
        .await;
        drain.listeners_released = rebound.is_ok();
        let cache_removed = std::fs::remove_file(&self.cache)
            .map_err(|error| format!("Remove owned synthetic cache: {error}"));
        drain.synthetic_data_removed = cache_removed.is_ok();
        drain.remaining_upstream_bodies = self.state.active.load(Ordering::SeqCst);
        drain.remaining_tickets = self.state.tickets.lock().unwrap().len();
        drain.elapsed_ms = millis(started.elapsed());
        let history = self
            .backend
            .history()
            .statistics()
            .map_err(|e| e.to_string());
        let result = active_cancel
            .and(runtime)
            .and(upstream)
            .and(rebound)
            .and(cache_removed)
            .and_then(|()| {
                require(
                    history?["entries"] == 0,
                    "store:false requests unexpectedly retained response history",
                )?;
                require(
                    drain.remaining_upstream_bodies == 0 && drain.remaining_tickets == 0,
                    "Owned inference work survived drain",
                )?;
                Ok(Evidence {
                    upstream_posts: 1,
                    cancellations: 1,
                    frames: 2,
                    ..Evidence::default()
                })
            });
        (drain, result)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.origin_shutdown.cancel();
        let _ = std::fs::remove_file(&self.cache);
    }
}

async fn origin_health(State(state): State<Arc<Upstream>>) -> Json<Value> {
    state.metadata_gets.fetch_add(1, Ordering::SeqCst);
    Json(json!({"message": "MAI LLM Proxy server is running"}))
}

async fn origin_models(State(state): State<Arc<Upstream>>) -> Json<Value> {
    state.metadata_gets.fetch_add(1, Ordering::SeqCst);
    Json(json!({"data": [{"id": "fixture-model"}]}))
}

async fn origin_unknown(State(state): State<Arc<Upstream>>) -> StatusCode {
    state.unexpected.fetch_add(1, Ordering::SeqCst);
    StatusCode::NOT_FOUND
}

async fn origin_inference(State(state): State<Arc<Upstream>>, request: Request<Body>) -> Response {
    state.posts.fetch_add(1, Ordering::SeqCst);
    let response = async {
        require(
            !request.headers().contains_key("authorization")
                && !request.headers().contains_key("cookie")
                && !request.headers().contains_key("x-api-key"),
            "Unexpected upstream credential header",
        )?;
        let permit = state
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|e| e.to_string())?;
        let bytes = tokio::time::timeout(
            REQUEST_TIMEOUT,
            to_bytes(request.into_body(), REQUEST_BYTES),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let id = value["input"]
            .as_str()
            .ok_or("Synthetic input must be a ticket ID")?;
        let ticket = state
            .tickets
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or("Unknown/replayed upstream ticket")?;
        if ticket.calls.fetch_add(1, Ordering::SeqCst) != 0 {
            state.duplicates.fetch_add(1, Ordering::SeqCst);
            return Err("Synthetic request was retried or replayed.".into());
        }
        require(
            value["model"] == "fixture-model" && value["store"] == false,
            "Unexpected synthetic request model or storage policy",
        )?;
        let stream = !matches!(ticket.mode, Mode::Json | Mode::Unavailable);
        require(
            value["stream"] == stream,
            "Upstream stream selection changed",
        )?;
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak_active.fetch_max(active, Ordering::SeqCst);
        let lease = Lease {
            state: state.clone(),
            ticket: ticket.clone(),
            finished: false,
            _permit: permit,
        };
        let (status, content_type, chunks) = match ticket.mode {
            Mode::Json => (
                StatusCode::OK,
                "application/json",
                vec![serde_json::to_vec(&resource(id)).map_err(|e| e.to_string())?],
            ),
            Mode::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                vec![br#"{"error":{"message":"synthetic unavailable; never retry"}}"#.to_vec()],
            ),
            Mode::Hold | Mode::Malformed => (
                StatusCode::OK,
                "text/event-stream",
                vec![support::frames(&[created(id)])],
            ),
            _ => (
                StatusCode::OK,
                "text/event-stream",
                events(id)
                    .iter()
                    .map(|event| support::frames(std::slice::from_ref(event)))
                    .collect(),
            ),
        };
        let body = async_stream::stream! {
            let mut lease = lease;
            for (index, chunk) in chunks.into_iter().enumerate() {
                if index > 0 && matches!(ticket.mode, Mode::Sse | Mode::Slow) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                yield Ok::<_, Infallible>(Bytes::from(chunk));
            }
            if ticket.mode == Mode::Hold {
                std::future::pending::<()>().await;
            }
            if ticket.mode == Mode::Malformed {
                ticket.gate.cancelled().await;
                yield Ok(Bytes::from_static(MALFORMED_SSE));
                // Rejection must release the upstream body rather than wait for a convenient EOF.
                std::future::pending::<()>().await;
            }
            lease.finished = true;
            drop(lease);
        };
        Ok::<_, String>(
            (
                status,
                [("content-type", content_type)],
                Body::from_stream(body),
            )
                .into_response(),
        )
    }
    .await;
    match response {
        Ok(response) => response,
        Err(_) => {
            state.unexpected.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":{"message":"synthetic fixture invariant failed"}})),
            )
                .into_response()
        }
    }
}

fn response_id(id: &str) -> String {
    format!("resp_{id}")
}
fn item_id(id: &str) -> String {
    format!("msg_{}", response_id(id))
}
fn delta(id: &str, index: usize) -> String {
    format!("{id}:{index:02}:{}|", "x".repeat(64))
}
fn text(id: &str) -> String {
    (0..DELTAS).map(|i| delta(id, i)).collect()
}

fn resource(id: &str) -> Value {
    let mut value = support::response(&response_id(id), &text(id));
    value["model"] = json!("fixture-model");
    value
}

fn created(id: &str) -> Value {
    let mut value = resource(id);
    value["status"] = json!("in_progress");
    value["output"] = json!([]);
    json!({"type": "response.created", "response": value})
}

fn events(id: &str) -> Vec<Value> {
    let mut result = vec![
        created(id),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {
            "id": item_id(id), "type": "message", "role": "assistant", "status": "in_progress", "content": [],
        }}),
    ];
    for index in 0..DELTAS {
        result.push(
            json!({"type": "response.output_text.delta", "output_index": 0,
            "content_index": 0, "item_id": format!("drift_{id}"), "delta": delta(id, index)}),
        );
    }
    let mut terminal = resource(id);
    terminal["id"] = json!(format!("terminal_{id}"));
    terminal["output"][0]["id"] = json!(format!("terminal_item_{id}"));
    result.push(json!({"type": "response.output_item.done", "output_index": 0, "item": terminal["output"][0]}));
    result.push(json!({"type": "response.completed", "response": terminal}));
    result
}

fn decoder() -> SseDecoder {
    SseDecoder::new(StreamLimits {
        max_event_bytes: RESPONSE_BYTES,
        max_stream_bytes: RESPONSE_BYTES,
        max_items: 32,
    })
}

async fn first_event(response: &mut reqwest::Response) -> Check<Vec<SseEvent>> {
    let mut decoder = decoder();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Early EOF before first event")?;
        let events = decoder.push(&chunk).map_err(|e| e.to_string())?;
        if !events.is_empty() {
            return Ok(events);
        }
    }
}

async fn read_json(mut response: reqwest::Response) -> Check<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        require(
            bytes.len() + chunk.len() <= RESPONSE_BYTES,
            "JSON consumer byte bound exceeded",
        )?;
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

async fn expect_error(response: reqwest::Response, status: u16, code: &str) -> Check<Value> {
    require(
        response.status().as_u16() == status,
        &format!("Expected HTTP {status}, received {}", response.status()),
    )?;
    let value = read_json(response).await?;
    require(
        value["error"]["type"] == code
            && value["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
        &format!("Expected shaped {code} rejection, received {value}"),
    )?;
    Ok(value)
}

async fn read_events(mut response: reqwest::Response, slow: bool) -> Check<Vec<SseEvent>> {
    let mut decoder = decoder();
    let mut events = Vec::new();
    loop {
        if slow {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? else {
            break;
        };
        events.extend(decoder.push(&chunk).map_err(|e| e.to_string())?);
        require(events.len() <= DELTAS + 8, "SSE event count bound exceeded")?;
    }
    events.extend(decoder.finish().map_err(|e| e.to_string())?);
    Ok(events)
}

fn validate_resource(value: &Value, id: &str) -> Check<()> {
    require(
        value["id"] == response_id(id)
            && value["object"] == "response"
            && value["model"] == "fixture-model"
            && value["status"] == "completed"
            && value["created_at"] == 1
            && value["output"]
                .as_array()
                .is_some_and(|items| items.len() == 1)
            && value["output"][0]["id"] == item_id(id)
            && value["output"][0]["type"] == "message"
            && value["output"][0]["role"] == "assistant"
            && value["output"][0]["status"] == "completed"
            && value["output"][0]["content"]
                .as_array()
                .is_some_and(|parts| parts.len() == 1)
            && value["output"][0]["content"][0]["type"] == "output_text"
            && value["output"][0]["content"][0]["text"] == text(id)
            && value["output"][0]["content"][0]["annotations"] == json!([])
            && value["usage"] == json!({"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}),
        "Response shape/text/usage or stable response/item identity mismatch",
    )
}

fn validate_events(events: &[SseEvent], id: &str) -> Check<()> {
    require(events.len() == DELTAS + 4, "SSE frame count mismatch")?;
    let mut reconstructed = String::new();
    for (index, event) in events.iter().enumerate() {
        let data = event
            .data
            .as_ref()
            .ok_or("Unexpected comment/DONE instead of JSON SSE")?;
        let expected = match index {
            0 => "response.created",
            1 => "response.output_item.added",
            i if i == DELTAS + 2 => "response.output_item.done",
            i if i == DELTAS + 3 => "response.completed",
            _ => "response.output_text.delta",
        };
        require(
            event.kind == expected && data["type"] == expected,
            "SSE event order/type mismatch",
        )?;
        match expected {
            "response.created" => require(
                data["response"]["id"] == response_id(id),
                "Created response identity",
            )?,
            "response.completed" => validate_resource(&data["response"], id)?,
            "response.output_item.added" | "response.output_item.done" => {
                require(
                    data["item"]["id"] == item_id(id) && data["output_index"] == 0,
                    "Stable SSE item identity/index",
                )?;
            }
            _ => {
                require(
                    data["item_id"] == item_id(id)
                        && data["output_index"] == 0
                        && data["content_index"] == 0
                        && data["delta"] == delta(id, index - 2),
                    "SSE delta text or stable identity mismatch",
                )?;
                reconstructed.push_str(data["delta"].as_str().unwrap());
            }
        }
    }
    require(reconstructed == text(id), "Reconstructed SSE text mismatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validators_reject_plausible_but_wrong_output() {
        let mut value = resource("test");
        validate_resource(&value, "test").unwrap();
        value["output"][0]["content"][0]["text"] = json!("wrong");
        assert!(validate_resource(&value, "test").is_err());
        value = resource("test");
        value["id"] = json!("wrong");
        assert!(validate_resource(&value, "test").is_err());
        let mut native = adapter_protocol::identity::NativeStream::default();
        let good = events("test")
            .into_iter()
            .map(|event| {
                native
                    .push(SseEvent::json(
                        event["type"].as_str().unwrap(),
                        event.clone(),
                    ))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        validate_events(&good, "test").unwrap();
        assert!(validate_events(&good[..good.len() - 1], "test").is_err());
        let mut duplicate = good;
        duplicate[3] = duplicate[2].clone();
        assert!(validate_events(&duplicate, "test").is_err());
    }
}
