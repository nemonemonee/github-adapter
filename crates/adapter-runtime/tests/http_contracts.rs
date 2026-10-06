use adapter_runtime::backend::{Backend, BackendConfig};
use adapter_runtime::context::RequestContext;
use adapter_runtime::server::{ServerOptions, serve};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

struct FixtureState {
    inference_calls: AtomicUsize,
    received_client_credential: AtomicBool,
    started: Notify,
    disconnected: Notify,
    last_request: tokio::sync::Mutex<Option<Value>>,
}

struct Fixture {
    endpoint: String,
    upstream: Arc<FixtureState>,
    client: reqwest::Client,
    shutdown: CancellationToken,
    origin_shutdown: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(mut options: ServerOptions) -> Self {
        options.inference_timeout = Duration::from_secs(5);
        options.shutdown_timeout = Duration::from_secs(1);
        let upstream = Arc::new(FixtureState {
            inference_calls: AtomicUsize::new(0),
            received_client_credential: AtomicBool::new(false),
            started: Notify::new(),
            disconnected: Notify::new(),
            last_request: tokio::sync::Mutex::new(None),
        });
        let origin = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin_address = origin.local_addr().unwrap();
        let stopping = CancellationToken::new();
        let origin_stopping = CancellationToken::new();
        let origin_stop = origin_stopping.clone();
        let origin_state = upstream.clone();
        let upstream_task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = origin_stop.cancelled() => break,
                    accepted = origin.accept() => {
                        let (stream, _) = accepted.unwrap();
                        connections.spawn(upstream_request(stream, origin_state.clone()));
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("models_cache.json");
        std::fs::write(
            &cache,
            serde_json::to_vec(&json!({"models": [{
                "slug": "fixture-model", "display_name": "Fixture model",
                "context_window": 32000, "supported_reasoning_levels": [{"effort": "low"}],
            }]}))
            .unwrap(),
        )
        .unwrap();
        let backend = Backend::connect(
            BackendConfig::Mai {
                upstream: format!("http://{origin_address}"),
                models_cache: cache,
            },
            RequestContext::new(Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server_stop = stopping.clone();
        let server_task = tokio::spawn(async move {
            serve(listener, backend, options, server_stop)
                .await
                .unwrap();
        });
        Self {
            endpoint,
            upstream,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            shutdown: stopping,
            origin_shutdown: origin_stopping,
            tasks: vec![upstream_task, server_task],
            _directory: directory,
        }
    }

    async fn slow_client(&self) -> TcpStream {
        let address = self.endpoint.strip_prefix("http://").unwrap();
        let mut client = TcpStream::connect(address).await.unwrap();
        let body =
            serde_json::to_string(&json!({"model": "fixture-model", "input": "slow"})).unwrap();
        client.write_all(format!(
            "POST /responses HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
        ).as_bytes()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), self.upstream.started.notified())
            .await
            .unwrap();
        client
    }

    async fn stop(mut self) {
        self.shutdown.cancel();
        let server = self.tasks.pop().unwrap();
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        self.origin_shutdown.cancel();
        for task in self.tasks.drain(..) {
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.origin_shutdown.cancel();
    }
}

async fn upstream_request(mut stream: TcpStream, state: Arc<FixtureState>) {
    let mut reader = BufReader::new(&mut stream);
    let mut first = String::new();
    if reader.read_line(&mut first).await.unwrap_or(0) == 0 {
        return;
    }
    let path = first.split_whitespace().nth(1).unwrap().to_string();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("content-length:") {
            length = line
                .split(':')
                .nth(1)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
        }
        if lower.starts_with("authorization:") || lower.starts_with("cookie:") {
            state
                .received_client_credential
                .store(true, Ordering::SeqCst);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    drop(reader);
    let response = match path.as_str() {
        "/" => json!({"message": "MAI LLM Proxy server is running"}),
        "/v1/models" => json!({"data": [{"id": "fixture-model"}]}),
        "/v1/responses" | "/responses" => {
            let request: Value = serde_json::from_slice(&body).unwrap();
            *state.last_request.lock().await = Some(request.clone());
            let count = state.inference_calls.fetch_add(1, Ordering::SeqCst);
            if request["input"] == "slow" {
                state.started.notify_one();
                let mut byte = [0];
                match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await {
                    Ok(Ok(0)) | Ok(Err(_)) => state.disconnected.notify_one(),
                    _ => {}
                }
                return;
            }
            let item = json!({"id": format!("item-{count}"), "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "fixture answer", "annotations": []}]});
            let resource = json!({"id": format!("response-{count}"), "object": "response", "model": "fixture-model",
                "created_at": 1, "status": "completed", "output": [item.clone()],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}});
            if request["stream"] == true {
                let events = [
                    json!({"type": "response.created", "response": {"id": format!("response-{count}"), "status": "in_progress", "output": []}}),
                    json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": format!("item-{count}"), "type": "message", "role": "assistant", "content": []}}),
                    json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": format!("item-{count}"), "delta": "fixture answer"}),
                    json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
                    json!({"type": "response.completed", "response": resource}),
                ];
                let bytes = events
                    .iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().unwrap()
                        )
                    })
                    .collect::<String>();
                send(&mut stream, bytes.as_bytes(), "text/event-stream").await;
                return;
            }
            resource
        }
        _ => panic!("Unexpected synthetic upstream path: {path}"),
    };
    send(
        &mut stream,
        &serde_json::to_vec(&response).unwrap(),
        "application/json",
    )
    .await;
}

async fn send(stream: &mut TcpStream, body: &[u8], content_type: &str) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(header.as_bytes()).await.is_ok() {
        let _ = stream.write_all(body).await;
    }
}

#[tokio::test]
async fn native_http_json_sse_history_and_credentials_keep_the_same_contract() {
    let fixture = Fixture::new(ServerOptions::default()).await;
    let health: Value = fixture
        .client
        .get(format!("{}/health", fixture.endpoint))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["provider"], "mai");
    let response = fixture
        .client
        .post(format!("{}/responses", fixture.endpoint))
        .header("authorization", "Bearer synthetic-client-credential")
        .header("cookie", "synthetic=client-cookie")
        .json(&json!({"model": "fixture-model", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let first: Value = response.json().await.unwrap();
    let saved: Value = fixture
        .client
        .get(format!(
            "{}/responses/{}",
            fixture.endpoint,
            first["id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved, first);
    let stream = fixture
        .client
        .post(format!("{}/v1/responses", fixture.endpoint))
        .json(&json!({"model": "fixture-model", "input": "hello", "stream": true, "store": false}))
        .send()
        .await
        .unwrap();
    assert!(stream.status().is_success());
    let body = stream.text().await.unwrap();
    assert!(body.contains("response.output_text.delta"));
    assert!(body.contains("response.completed"));
    assert!(
        !fixture
            .upstream
            .received_client_credential
            .load(Ordering::SeqCst)
    );
    fixture.stop().await;
}

#[tokio::test]
async fn invalid_requests_do_not_reach_the_upstream() {
    let fixture = Fixture::new(ServerOptions {
        max_request_bytes: 1024,
        ..ServerOptions::default()
    })
    .await;
    for (path, value, expected) in [
        ("/responses", json!([]), 400),
        (
            "/responses?unexpected=true",
            json!({"model": "fixture-model", "input": "hello"}),
            400,
        ),
        (
            "/responses",
            json!({"model": "fixture-model", "input": "x".repeat(2048)}),
            413,
        ),
    ] {
        let response = fixture
            .client
            .post(format!("{}{path}", fixture.endpoint))
            .json(&value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    let response = fixture
        .client
        .post(format!("{}/responses", fixture.endpoint))
        .header("origin", "https://untrusted.invalid")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
    assert_eq!(fixture.upstream.inference_calls.load(Ordering::SeqCst), 0);
    fixture.stop().await;
}

#[tokio::test]
async fn compressed_requests_reject_oversized_zstd_windows_before_inference() {
    let fixture = Fixture::new(ServerOptions::default()).await;
    let payload = serde_json::to_vec(&json!({"model":"fixture-model","input":"hello"})).unwrap();
    let frame = |window_log: u8| {
        // A valid, unchecksummed Zstd frame with one raw final block and an
        // explicit window descriptor. The decoded JSON is small in both cases.
        let mut bytes = vec![0x28, 0xb5, 0x2f, 0xfd, 0, (window_log - 10) << 3];
        let block = ((payload.len() as u32) << 3) | 1;
        bytes.extend_from_slice(&block.to_le_bytes()[..3]);
        bytes.extend_from_slice(&payload);
        bytes
    };
    for (window, expected) in [(26, 200), (27, 400)] {
        let response = fixture
            .client
            .post(format!("{}/responses", fixture.endpoint))
            .header("content-type", "application/json")
            .header("content-encoding", "zstd")
            .body(frame(window))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    assert_eq!(fixture.upstream.inference_calls.load(Ordering::SeqCst), 1);
    fixture.stop().await;
}

#[tokio::test]
async fn client_disconnect_before_headers_cancels_upstream_work() {
    let fixture = Fixture::new(ServerOptions::default()).await;
    let client = fixture.slow_client().await;
    drop(client);
    tokio::time::timeout(
        Duration::from_secs(2),
        fixture.upstream.disconnected.notified(),
    )
    .await
    .expect("Downstream EOF must cancel the pending upstream request");
    fixture.stop().await;
}

#[tokio::test]
async fn admission_is_bounded_but_health_remains_available() {
    let fixture = Fixture::new(ServerOptions {
        max_active_requests: 1,
        ..ServerOptions::default()
    })
    .await;
    let client = fixture.slow_client().await;
    let response = fixture
        .client
        .post(format!("{}/responses", fixture.endpoint))
        .json(&json!({"model": "fixture-model", "input": "next"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 429);
    assert!(
        fixture
            .client
            .get(format!("{}/health", fixture.endpoint))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(fixture.upstream.inference_calls.load(Ordering::SeqCst), 1);
    drop(client);
    tokio::time::timeout(
        Duration::from_secs(2),
        fixture.upstream.disconnected.notified(),
    )
    .await
    .unwrap();
    fixture.stop().await;
}

#[tokio::test]
async fn shutdown_cancels_active_upstream_and_releases_the_listener() {
    let fixture = Fixture::new(ServerOptions::default()).await;
    let client = fixture.slow_client().await;
    let address = fixture
        .endpoint
        .strip_prefix("http://")
        .unwrap()
        .to_string();
    fixture.shutdown.cancel();
    tokio::time::timeout(
        Duration::from_secs(2),
        fixture.upstream.disconnected.notified(),
    )
    .await
    .unwrap();
    fixture.stop().await;
    drop(client);
    let rebound = TcpListener::bind(&address).await.unwrap();
    drop(rebound);
}

#[tokio::test]
async fn mai_anthropic_constraints_are_forwarded_or_rejected_never_silently_dropped() {
    let fixture = Fixture::new(ServerOptions::default()).await;
    let request = json!({
        "model": "fixture-model", "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 32, "temperature": 0.2, "top_p": 0.8,
        "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
    });
    let response = fixture
        .client
        .post(format!("{}/v1/messages?beta=true", fixture.endpoint))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let outgoing = fixture.upstream.last_request.lock().await.clone().unwrap();
    assert_eq!(outgoing["temperature"], 0.2);
    assert_eq!(outgoing["top_p"], 0.8);
    assert_eq!(outgoing["parallel_tool_calls"], false);
    let mut unsupported = request;
    unsupported["stop_sequences"] = json!(["END"]);
    let response = fixture
        .client
        .post(format!("{}/messages", fixture.endpoint))
        .json(&unsupported)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
    assert_eq!(fixture.upstream.inference_calls.load(Ordering::SeqCst), 1);
    fixture.stop().await;
}

#[tokio::test]
async fn buffered_anthropic_stream_keeps_its_response_writer_alive() {
    let fixture = Fixture::new(ServerOptions::default()).await;
    let response = fixture
        .client
        .post(format!("{}/messages", fixture.endpoint))
        .json(&json!({
            "model": "fixture-model", "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 32, "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body = response.text().await.unwrap();
    assert_eq!(body.matches("event: message_start\n").count(), 1);
    assert_eq!(body.matches("event: message_stop\n").count(), 1);
    assert_eq!(body.matches("fixture answer").count(), 1);
    assert!(!body.contains("event: error"));
    assert_eq!(fixture.upstream.inference_calls.load(Ordering::SeqCst), 1);
    fixture.stop().await;
}
