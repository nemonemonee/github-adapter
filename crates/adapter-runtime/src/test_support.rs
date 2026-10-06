//! Shared synthetic upstream fixtures; never compiled into the application.

use adapter_protocol::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
use url::Url;

#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn json(&self) -> Value {
        adapter_protocol::json::decode(&self.body, 2 * 1024 * 1024).unwrap()
    }

    pub fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }
}

pub(crate) struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub chunks: Vec<Vec<u8>>,
    pub gate: Option<Arc<Notify>>,
}

impl Reply {
    pub fn json(value: Value) -> Self {
        Self::raw(200, "application/json", serde_json::to_vec(&value).unwrap())
    }

    pub fn raw(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            headers: Vec::new(),
            chunks: vec![body],
            gate: None,
        }
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    pub fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    pub fn sse(events: &[Value]) -> Self {
        Self::raw(200, "text/event-stream", frames(events))
    }

    pub fn gated(first: Vec<u8>, last: Vec<u8>, gate: Arc<Notify>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            headers: Vec::new(),
            chunks: vec![first, last],
            gate: Some(gate),
        }
    }
}

pub(crate) struct Fixture {
    pub origin: Url,
    requests: Arc<Mutex<Vec<Recorded>>>,
    pub disconnected: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Fixture {
    pub async fn start(handler: impl Fn(&Recorded, &Url) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let disconnected = Arc::new(AtomicUsize::new(0));
        let task_requests = requests.clone();
        let task_disconnected = disconnected.clone();
        let handler = Arc::new(handler);
        let task_origin = origin.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept() => {
                        let Ok((stream, _)) = incoming else { break };
                        let requests = task_requests.clone();
                        let disconnected = task_disconnected.clone();
                        let handler = handler.clone();
                        let origin = task_origin.clone();
                        connections.spawn(async move {
                            let _ = serve_one(stream, requests, disconnected, handler, origin).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            origin,
            requests,
            disconnected,
            task,
        }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Handler = dyn Fn(&Recorded, &Url) -> Reply + Send + Sync;

async fn serve_one(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<Recorded>>>,
    disconnected: Arc<AtomicUsize>,
    handler: Arc<Handler>,
    origin: Url,
) -> std::io::Result<()> {
    let mut received = Vec::new();
    let split = loop {
        if let Some(index) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        received.extend_from_slice(&buffer[..count]);
        if received.len() > 64 * 1024 {
            return Err(std::io::Error::other("fixture header limit"));
        }
    };
    let text = String::from_utf8_lossy(&received[..split]);
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap().split_whitespace().collect::<Vec<_>>();
    let method = line[0].to_owned();
    let path = line[1].to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .get("content-length")
        .map(|length| length.parse::<usize>().unwrap())
        .unwrap_or(0);
    if length > 2 * 1024 * 1024 {
        return Err(std::io::Error::other("fixture body limit"));
    }
    while received.len() - split < length {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        received.extend_from_slice(&buffer[..count]);
    }
    let request = Recorded {
        method,
        path,
        headers,
        body: received[split..split + length].to_vec(),
    };
    requests.lock().unwrap().push(request.clone());
    let reply = handler(&request, &origin);
    let mut headers = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nConnection: close\r\n",
        reply.status, reply.content_type
    );
    if !reply
        .headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("content-length"))
    {
        headers.push_str(&format!(
            "Content-Length: {}\r\n",
            reply.chunks.iter().map(Vec::len).sum::<usize>()
        ));
    }
    for (key, value) in reply.headers {
        headers.push_str(&format!("{key}: {value}\r\n"));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    for (index, chunk) in reply.chunks.iter().enumerate() {
        if index > 0
            && let Some(gate) = &reply.gate
        {
            let mut byte = [0; 1];
            tokio::select! {
                _ = gate.notified() => {}
                _ = stream.read(&mut byte) => {
                    disconnected.fetch_add(1, Ordering::SeqCst);
                    return Ok(());
                }
            }
        }
        if stream.write_all(chunk).await.is_err() {
            disconnected.fetch_add(1, Ordering::SeqCst);
            return Ok(());
        }
        stream.flush().await?;
    }
    stream.shutdown().await
}

pub(crate) struct CacheFile {
    pub path: PathBuf,
}

impl CacheFile {
    pub fn new(value: &Value) -> Self {
        static FILES: AtomicU64 = AtomicU64::new(0);
        let directory = PathBuf::from("target").join("backend-contract-data");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!(
            "models-{}-{}.json",
            std::process::id(),
            FILES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        Self { path }
    }
}

impl Drop for CacheFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn frames(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|event| format!("data: {}\n\n", serde_json::to_string(event).unwrap()))
        .collect::<String>()
        .into_bytes()
}

pub(crate) fn model(id: &str, native: bool) -> Value {
    json!({
        "id": id, "name": id, "vendor": if native { "OpenAI" } else { "Anthropic" },
        "model_picker_enabled": true, "policy": {"state": "enabled"},
        "supported_endpoints": if native { json!(["/responses", "/chat/completions"]) } else { json!(["/chat/completions"]) },
        "capabilities": {
            "type": "chat",
            "limits": {"max_context_window_tokens": 32_000, "max_output_tokens": 1000},
            "supports": {"tool_calls": true, "parallel_tool_calls": true},
        },
    })
}

pub(crate) fn response(id: &str, text: &str) -> Value {
    json!({
        "id": id, "object": "response", "model": "m", "created_at": 1, "status": "completed",
        "output": [{
            "type": "message", "id": format!("msg_{id}"), "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
    })
}

pub(crate) fn authorization(origin: &Url, token: &str) -> Value {
    json!({
        "token": token,
        "expires_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600,
        "refresh_in": 1800,
        "endpoints": {"api": origin.as_str()},
    })
}
