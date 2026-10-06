use crate::backend::{Backend, EventStream};
use crate::context::{CancelOnDrop, RequestContext};
use crate::http_routes::Endpoint;
use adapter_protocol::anthropic;
use adapter_protocol::sse::SseEvent;
use adapter_protocol::{AdapterError, Result, Value};
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json, Router};
use futures_util::{Stream, StreamExt};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as ConnectionBuilder;
use hyper_util::service::TowerToHyperService;
use serde_json::json;
use std::convert::Infallible;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone)]
pub struct ServerOptions {
    pub max_active_requests: usize,
    pub max_request_bytes: usize,
    pub body_timeout: Duration,
    pub inference_timeout: Duration,
    pub header_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_active_requests: 8,
            max_request_bytes: 64 * 1024 * 1024,
            body_timeout: Duration::from_secs(30),
            inference_timeout: Duration::from_secs(600),
            header_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Clone)]
struct ServerState {
    backend: Arc<Backend>,
    options: ServerOptions,
    admission: Arc<Semaphore>,
}

#[derive(Clone)]
struct ConnectionCancellation(CancellationToken);

pub fn router(backend: Arc<Backend>, options: ServerOptions) -> Result<Router> {
    if options.max_active_requests == 0
        || options.max_active_requests > 128
        || options.max_request_bytes == 0
        || options.max_request_bytes > 64 * 1024 * 1024
        || options.body_timeout.is_zero()
        || options.inference_timeout.is_zero()
        || options.header_timeout.is_zero()
        || options.shutdown_timeout.is_zero()
    {
        return Err(AdapterError::invalid("Server limits must be positive."));
    }
    Ok(Router::new().fallback(dispatch).with_state(ServerState {
        backend,
        admission: Arc::new(Semaphore::new(options.max_active_requests)),
        options,
    }))
}

pub async fn serve(
    listener: TcpListener,
    backend: Arc<Backend>,
    options: ServerOptions,
    shutdown: CancellationToken,
) -> Result<()> {
    if !listener
        .local_addr()
        .map_err(|_| AdapterError::new(500, "listener_error", "Cannot inspect the listener."))?
        .ip()
        .is_loopback()
    {
        return Err(AdapterError::invalid(
            "The adapter listener must be loopback-only.",
        ));
    }
    let app = router(backend, options.clone())?;
    let connections = Arc::new(Semaphore::new(options.max_active_requests * 2 + 32));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::error!(%error, "HTTP connection task failed");
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|_| AdapterError::new(500, "accept_error", "The local listener could not accept a connection."))?;
                let permit = match connections.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!("Local connection limit reached; refusing a connection");
                        drop(stream);
                        continue;
                    }
                };
                stream.set_nodelay(true).map_err(|_| AdapterError::new(500, "socket_error", "Could not configure the local connection."))?;
                let connection_cancel = shutdown.child_token();
                let service = TowerToHyperService::new(
                    app.clone().layer(Extension(ConnectionCancellation(connection_cancel.clone()))),
                );
                let options = options.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let cancel_guard = connection_cancel.clone().drop_guard();
                    let mut builder = ConnectionBuilder::new(TokioExecutor::new());
                    builder.http1()
                        .timer(TokioTimer::new())
                        .header_read_timeout(options.header_timeout);
                    builder.http2().max_concurrent_streams(options.max_active_requests as u32);
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        result = connection.as_mut() => {
                            if result.is_err() {
                                tracing::debug!("HTTP connection closed with a transport error");
                            }
                        }
                        _ = connection_cancel.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            if tokio::time::timeout(options.shutdown_timeout, connection.as_mut()).await.is_err() {
                                tracing::warn!("HTTP connection exceeded its shutdown deadline");
                            }
                        }
                    }
                    drop(cancel_guard);
                });
            }
        }
    }
    drop(listener);
    if tokio::time::timeout(options.shutdown_timeout, async {
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "HTTP task failed during shutdown");
            }
        }
    })
    .await
    .is_err()
    {
        tracing::warn!("Aborting HTTP tasks after the shutdown deadline");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    Ok(())
}

fn error_response(error: AdapterError, anthropic: bool) -> Response {
    let status = match StatusCode::from_u16(error.status) {
        Ok(status) if status.is_client_error() || status.is_server_error() => status,
        _ => {
            tracing::error!(status = error.status, "Invalid internal error status");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    let mut payload = json!({"error": {"type": error.code, "message": error.message}});
    if anthropic {
        payload["type"] = json!("error");
    }
    (status, Json(payload)).into_response()
}

fn permitted_origin(headers: &HeaderMap) -> Result<()> {
    if let Some(origin) = headers.get("origin") {
        let origin = origin
            .to_str()
            .ok()
            .and_then(|value| Url::parse(value).ok());
        if !origin.is_some_and(|origin| {
            matches!(origin.scheme(), "http" | "https")
                && matches!(
                    origin.host_str(),
                    Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
                )
                && origin.username().is_empty()
                && origin.password().is_none()
        }) {
            return Err(AdapterError::new(
                403,
                "origin_rejected",
                "Cross-origin browser access is not enabled.",
            ));
        }
    }
    Ok(())
}

async fn payload(request: Request<Body>, options: &ServerOptions) -> Result<Value> {
    if request.headers().contains_key("transfer-encoding")
        || request.headers().get_all("content-length").iter().count() != 1
    {
        return Err(AdapterError::invalid(
            "Exactly one fixed Content-Length is required.",
        ));
    }
    let length = request.headers()["content-length"]
        .to_str()
        .ok()
        .and_then(|length| length.parse::<usize>().ok())
        .ok_or_else(|| AdapterError::invalid("Content-Length must be a nonnegative integer."))?;
    if length > options.max_request_bytes {
        return Err(AdapterError::new(
            413,
            "request_too_large",
            "The request exceeds the local byte limit.",
        ));
    }
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Err(AdapterError::new(
            415,
            "unsupported_media_type",
            "Inference requires application/json.",
        ));
    }
    let encoding = request
        .headers()
        .get("content-encoding")
        .map(|value| value.to_str().map(str::to_owned))
        .transpose()
        .map_err(|_| AdapterError::invalid("Invalid Content-Encoding."))?
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let bytes = tokio::time::timeout(
        options.body_timeout,
        to_bytes(request.into_body(), options.max_request_bytes),
    )
    .await
    .map_err(|_| {
        AdapterError::new(
            408,
            "request_timeout",
            "Timed out reading the request body.",
        )
    })?
    .map_err(|_| {
        AdapterError::invalid("The request body could not be read within its byte limit.")
    })?;
    if bytes.len() != length {
        return Err(AdapterError::invalid(
            "The request body does not match Content-Length.",
        ));
    }
    let maximum = options.max_request_bytes;
    let body = match encoding.as_str() {
        "" | "identity" => bytes.to_vec(),
        "zstd" => tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let mut decoder = zstd::stream::read::Decoder::new(bytes.as_ref())
                .map_err(|_| AdapterError::invalid("Invalid zstd request body."))?;
            decoder
                .window_log_max(26)
                .map_err(|_| AdapterError::invalid("Could not bound the zstd request window."))?;
            let mut decoded = Vec::new();
            decoder
                .take(maximum as u64 + 1)
                .read_to_end(&mut decoded)
                .map_err(|_| AdapterError::invalid("Invalid zstd request body."))?;
            if decoded.len() > maximum {
                return Err(AdapterError::new(
                    413,
                    "request_too_large",
                    "The decoded request exceeds the local byte limit.",
                ));
            }
            Ok(decoded)
        })
        .await
        .map_err(|_| {
            AdapterError::new(
                500,
                "decode_worker_error",
                "The body decoder could not complete.",
            )
        })??,
        _ => {
            return Err(AdapterError::invalid(
                "Only identity and zstd encodings are supported.",
            ));
        }
    };
    let value = adapter_protocol::json::decode(&body, maximum)?;
    if !value.is_object() {
        return Err(AdapterError::invalid(
            "The request body must be a JSON object.",
        ));
    }
    if value
        .get("stream")
        .is_some_and(|stream| !stream.is_boolean())
    {
        return Err(AdapterError::invalid("stream must be a boolean."));
    }
    Ok(value)
}

async fn dispatch(State(state): State<ServerState>, request: Request<Body>) -> Response {
    let cancellation = request
        .extensions()
        .get::<ConnectionCancellation>()
        .map(|connection| connection.0.child_token())
        .unwrap_or_default();
    let path = request.uri().path().to_string();
    let endpoint = Endpoint::classify(&path);
    let query = request.uri().query().unwrap_or("").to_string();
    let method = request.method().clone();
    let anthropic = endpoint == Endpoint::Messages;
    if let Err(error) = permitted_origin(request.headers()) {
        return error_response(error, anthropic);
    }
    if method == Method::OPTIONS {
        return (
            StatusCode::NO_CONTENT,
            [("Allow", "GET, POST, DELETE, OPTIONS")],
        )
            .into_response();
    }
    if method == Method::GET {
        if endpoint == Endpoint::Health {
            return Json(state.backend.health()).into_response();
        }
        if endpoint == Endpoint::Models {
            let context = match RequestContext::with_cancellation(
                Duration::from_secs(10),
                cancellation.clone(),
            ) {
                Ok(context) => context,
                Err(error) => return error_response(error, false),
            };
            return match state.backend.model_catalog(&context).await {
                Ok(catalog) => Json(catalog).into_response(),
                Err(error) => error_response(error, false),
            };
        }
        if endpoint == Endpoint::Responses {
            let websocket = request
                .headers()
                .get("upgrade")
                .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
            return error_response(
                AdapterError::new(
                    if websocket { 426 } else { 405 },
                    "unsupported_transport",
                    if websocket {
                        "WebSocket transport is not supported."
                    } else {
                        "Use POST for Responses."
                    },
                ),
                false,
            );
        }
    }
    if method == Method::GET || method == Method::DELETE {
        let result = async {
            if !query.is_empty() {
                return Err(AdapterError::invalid(
                    "History endpoints do not accept query parameters.",
                ));
            }
            let id = endpoint.history_id()?.ok_or_else(|| {
                AdapterError::new(404, "not_found_error", "Unknown adapter endpoint.")
            })?;
            let _permit = state.admission.clone().try_acquire_owned().map_err(|_| {
                AdapterError::new(
                    429,
                    "rate_limit_error",
                    "The local concurrency limit is reached.",
                )
            })?;
            let context =
                RequestContext::with_cancellation(Duration::from_secs(30), cancellation.clone())?;
            state
                .backend
                .response_history(&id, method == Method::DELETE, context)
                .await
        }
        .await;
        return match result {
            Ok(value) => Json(value).into_response(),
            Err(error) => error_response(error, false),
        };
    }
    if method != Method::POST {
        return error_response(
            AdapterError::new(405, "method_not_allowed", "Unsupported HTTP method."),
            false,
        );
    }
    let permit = match state.admission.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return error_response(
                AdapterError::new(
                    429,
                    "rate_limit_error",
                    "The local concurrency limit is reached.",
                ),
                anthropic,
            );
        }
    };
    let body_context =
        match RequestContext::with_cancellation(state.options.body_timeout, cancellation.clone()) {
            Ok(context) => context,
            Err(error) => return error_response(error, anthropic),
        };
    let payload = match body_context.bounded(payload(request, &state.options)).await {
        Ok(payload) => payload,
        Err(error) => return error_response(error, anthropic),
    };
    if !endpoint.inference_query_allowed(&query) {
        return error_response(
            AdapterError::invalid("Query parameters are not supported on inference endpoints."),
            anthropic,
        );
    }
    let context =
        match RequestContext::with_cancellation(state.options.inference_timeout, cancellation) {
            Ok(context) => context,
            Err(error) => return error_response(error, anthropic),
        };
    let guard = context.cancel_on_drop();
    let result = inference(&state.backend, endpoint, payload, context.child()).await;
    match result {
        Ok(Inference::Json(value)) => Json(value).into_response(),
        Ok(Inference::Stream(mut stream)) => {
            let first = match context
                .bounded(async {
                    stream.next().await.unwrap_or_else(|| {
                        Err(AdapterError::upstream(
                            "The upstream event stream was empty.",
                        ))
                    })
                })
                .await
            {
                Ok(event) => event,
                Err(error) => return error_response(error, anthropic),
            };
            (
                [
                    ("Content-Type", "text/event-stream"),
                    ("Cache-Control", "no-cache"),
                ],
                Body::from_stream(OutgoingEvents {
                    inner: stream,
                    first: Some(first),
                    sequence: None,
                    finished: false,
                    guard: Some(guard),
                    _permit: permit,
                }),
            )
                .into_response()
        }
        Err(error) => error_response(error, anthropic),
    }
}

enum Inference {
    Json(Value),
    Stream(EventStream),
}

async fn inference(
    backend: &Arc<Backend>,
    endpoint: Endpoint<'_>,
    payload: Value,
    context: RequestContext,
) -> Result<Inference> {
    let streaming = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match endpoint {
        Endpoint::Responses if streaming => Ok(Inference::Stream(
            backend.clone().stream_responses(payload, context).await?,
        )),
        Endpoint::Responses => Ok(Inference::Json(
            backend.complete_responses(payload, context).await?,
        )),
        Endpoint::Compaction => Ok(Inference::Json(
            backend.compact_responses(payload, context).await?,
        )),
        Endpoint::Chat if streaming => Ok(Inference::Stream(
            backend.clone().stream_chat(payload, context).await?,
        )),
        Endpoint::Chat => Ok(Inference::Json(
            backend.complete_chat(payload, context).await?,
        )),
        Endpoint::Messages => {
            let message = backend.complete_messages(payload, context).await?;
            if streaming {
                Ok(Inference::Stream(Box::pin(futures_util::stream::iter(
                    anthropic::events(&message)?.into_iter().map(Ok),
                ))))
            } else {
                Ok(Inference::Json(message))
            }
        }
        _ => Err(AdapterError::new(
            404,
            "not_found_error",
            "Unknown adapter endpoint.",
        )),
    }
}

struct OutgoingEvents {
    inner: EventStream,
    first: Option<SseEvent>,
    sequence: Option<u64>,
    finished: bool,
    guard: Option<CancelOnDrop>,
    _permit: OwnedSemaphorePermit,
}

impl Stream for OutgoingEvents {
    type Item = std::result::Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        let event = if let Some(first) = self.first.take() {
            Ok(first)
        } else {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(event)) => event,
                Poll::Ready(None) => {
                    self.finished = true;
                    self.guard.take();
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        };
        match event.and_then(|event| {
            if let Some(sequence) = event
                .data
                .as_ref()
                .and_then(|data| data.get("sequence_number"))
                .and_then(Value::as_u64)
            {
                self.sequence = Some(sequence);
            }
            event.to_bytes()
        }) {
            Ok(bytes) => Poll::Ready(Some(Ok(Bytes::from(bytes)))),
            Err(error) => {
                self.finished = true;
                self.guard.take();
                self.inner = Box::pin(futures_util::stream::empty());
                tracing::warn!(
                    status = error.status,
                    code = error.code,
                    "Upstream stream failed"
                );
                let mut data = json!({
                    "type": "error",
                    "code": error.code, "message": error.message,
                    "error": {"type": "api_error", "message": error.message},
                });
                let next_sequence = self
                    .sequence
                    .map_or(Some(0), |sequence| sequence.checked_add(1));
                if let Some(sequence) = next_sequence {
                    data["sequence_number"] = json!(sequence);
                } else {
                    tracing::error!("Cannot represent the next SSE error sequence number");
                }
                let body = format!("event: error\ndata: {data}\n\n");
                Poll::Ready(Some(Ok(Bytes::from(body))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_origins_do_not_enable_cross_site_inference() {
        let mut headers = HeaderMap::new();
        assert!(permitted_origin(&headers).is_ok());
        headers.insert("origin", "https://untrusted.invalid".parse().unwrap());
        assert_eq!(permitted_origin(&headers).unwrap_err().status, 403);
        headers.insert("origin", "http://127.0.0.1:5001".parse().unwrap());
        assert!(permitted_origin(&headers).is_ok());
    }
}
