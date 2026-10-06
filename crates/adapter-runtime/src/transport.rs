//! Shared HTTP and redaction infrastructure, independent of provider policy.

use crate::context::RequestContext;
use adapter_protocol::{AdapterError, Result, Value};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, RequestBuilder, Response};
use std::fmt;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::{Host, Url};
use zstd::stream::raw::{DParameter, Decoder, Operation};

pub(crate) const USER_AGENT: &str = "GitHub-Adapter/2.0";
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const AUTH_BYTES: usize = 1024 * 1024;
pub(crate) const MODEL_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const CACHE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const HEALTH_BYTES: usize = 64 * 1024;
pub(crate) const INFERENCE_BYTES: usize = 64 * 1024 * 1024;
const DECODE_CHUNK: usize = 64 * 1024;

pub(crate) type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

#[derive(Clone, Default)]
pub(crate) struct Redaction(Vec<Arc<str>>);

impl Redaction {
    pub(crate) fn new(secrets: &[&str]) -> Self {
        let mut secrets: Vec<Arc<str>> = secrets
            .iter()
            .filter(|secret| !secret.is_empty())
            .map(|secret| Arc::from(*secret))
            .collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        Self(secrets)
    }

    pub(crate) fn text(&self, text: &str) -> String {
        self.0.iter().fold(text.to_owned(), |text, secret| {
            text.replace(secret.as_ref(), "[redacted]")
        })
    }

    pub(crate) fn error(&self, mut error: AdapterError) -> AdapterError {
        error.message = self.text(&error.message);
        error.code = self.text(&error.code);
        error
    }
}

impl fmt::Debug for Redaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Redaction([redacted])")
    }
}

pub(crate) fn client(loopback: bool) -> Result<Client> {
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONTROL_TIMEOUT)
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(Duration::from_secs(90))
        .user_agent(USER_AGENT)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd();
    if loopback {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|_| {
        AdapterError::new(
            500,
            "transport_unavailable",
            "Could not create the verified HTTP client.",
        )
    })
}

pub(crate) fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

pub(crate) fn endpoint(value: &str) -> Result<Url> {
    let invalid = || {
        AdapterError::invalid(
            "The upstream must be an unambiguous HTTP(S) URL without credentials, query, or fragment.",
        )
    };
    if value.trim() != value
        || value
            .chars()
            .any(|character| character.is_control() || character == '\\')
    {
        return Err(invalid());
    }
    let url = Url::parse(value).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err(invalid());
    }
    // URL parsers normalize dot segments. Reject them before normalization.
    let authority = value.split_once("://").ok_or_else(invalid)?.1;
    let path = authority.find('/').map_or("", |index| &authority[index..]);
    for segment in path.split('/') {
        let mut decoded = Vec::new();
        let mut bytes = segment.as_bytes().iter().copied();
        while let Some(byte) = bytes.next() {
            let byte = if byte == b'%' {
                let high = bytes.next().and_then(hex).ok_or_else(invalid)?;
                let low = bytes.next().and_then(hex).ok_or_else(invalid)?;
                let decoded = high * 16 + low;
                if matches!(decoded, b'/' | b'\\' | b'%') {
                    return Err(invalid());
                }
                decoded
            } else {
                byte
            };
            if byte < 32 || byte == 127 {
                return Err(invalid());
            }
            decoded.push(byte);
        }
        if decoded == b"." || decoded == b".." {
            return Err(invalid());
        }
    }
    Ok(url)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn endpoint_name(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_owned()
}

pub(crate) fn append_path(base: &Url, path: &str) -> Result<Url> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['?', '#', '\\', '%'])
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return Err(AdapterError::invalid(
            "Only fixed local API paths are permitted.",
        ));
    }
    Url::parse(&format!("{}{path}", endpoint_name(base)))
        .map_err(|_| AdapterError::invalid("Invalid upstream API path."))
}

pub(crate) fn unsigned(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| value.try_into().ok()))
}

pub(crate) fn positive(value: &Value, label: &str) -> Result<u64> {
    unsigned(value)
        .filter(|value| *value > 0)
        .ok_or_else(|| AdapterError::invalid(format!("{label} must be a positive integer.")))
}

pub(crate) fn bearer(scheme: &str, token: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("{scheme} {token}")).map_err(|_| {
        AdapterError::upstream("The selected credential cannot be used in an HTTP header.")
    })?;
    value.set_sensitive(true);
    Ok(value)
}

pub(crate) fn timeout(context: &RequestContext, ceiling: Option<Duration>) -> Result<Duration> {
    context.check()?;
    let remaining = context
        .deadline
        .saturating_duration_since(tokio::time::Instant::now());
    Ok(ceiling.map_or(remaining, |ceiling| ceiling.min(remaining)))
}

pub(crate) async fn send(
    request: RequestBuilder,
    context: &RequestContext,
    operation: &'static str,
) -> Result<Response> {
    context
        .bounded(async {
            request.send().await.map_err(|_| {
                AdapterError::upstream(format!(
                    "{operation} could not connect to or read the selected upstream. No request was replayed."
                ))
            })
        })
        .await
}

pub(crate) fn error_status(status: u16) -> u16 {
    if (400..=599).contains(&status) {
        status
    } else {
        502
    }
}

pub(crate) fn message(payload: &Value, fallback: &str) -> String {
    payload
        .get("error")
        .filter(|value| value.is_object())
        .and_then(|error| error.get("message"))
        .or_else(|| payload.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

pub(crate) fn content_type(response: &Response) -> Result<String> {
    response
        .headers()
        .get(CONTENT_TYPE)
        .map_or(Ok(String::new()), |value| {
            value
                .to_str()
                .map(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .map_err(|_| {
                    AdapterError::upstream("The upstream returned an invalid Content-Type.")
                })
        })
}

pub(crate) fn body(
    mut response: Response,
    context: RequestContext,
    limit: usize,
    operation: &'static str,
) -> Result<ByteStream> {
    let status = error_status(response.status().as_u16());
    let failure = move |reason: &str| {
        AdapterError::new(status, "upstream_error", format!("{operation} {reason}"))
    };
    let lengths: Vec<_> = response.headers().get_all(CONTENT_LENGTH).iter().collect();
    if lengths.len() > 1 {
        return Err(failure("returned ambiguous Content-Length."));
    }
    if let Some(length) = lengths.first() {
        let value = length
            .to_str()
            .ok()
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| failure("returned invalid Content-Length."))?;
        if value > limit as u64 {
            return Err(failure("returned an oversized response."));
        }
    }
    let encodings: Vec<_> = response
        .headers()
        .get_all(CONTENT_ENCODING)
        .iter()
        .collect();
    if encodings.len() > 1 {
        return Err(failure("returned ambiguous Content-Encoding."));
    }
    let encoding = encodings.first().map_or(Ok(String::new()), |encoding| {
        encoding
            .to_str()
            .map(|value| value.trim().to_ascii_lowercase())
            .map_err(|_| failure("returned invalid Content-Encoding."))
    })?;
    let mut decoder = match encoding.as_str() {
        "" | "identity" => None,
        "zstd" => {
            let mut decoder =
                Decoder::new().map_err(|_| failure("could not initialize zstd decoding."))?;
            decoder
                .set_parameter(DParameter::WindowLogMax(26))
                .map_err(|_| failure("could not bound the zstd decoder window."))?;
            Some(decoder)
        }
        _ => {
            return Err(failure(
                "returned unsupported Content-Encoding; only identity and zstd are supported.",
            ));
        }
    };
    Ok(Box::pin(async_stream::try_stream! {
        let mut received = 0usize;
        let mut decoded = 0usize;
        let mut finished_frame = false;
        let mut output = vec![0; DECODE_CHUNK];
        while let Some(chunk) = context.bounded(async {
            response.chunk().await.map_err(|_| failure("disconnected while its response was being read."))
        }).await? {
            context.check()?;
            received = received.checked_add(chunk.len())
                .filter(|size| *size <= limit)
                .ok_or_else(|| failure("returned an oversized encoded response."))?;
            if chunk.is_empty() {
                continue;
            }
            if let Some(decoder) = &mut decoder {
                let mut offset = 0;
                let mut drain = false;
                while offset < chunk.len() || drain {
                    context.check()?;
                    if finished_frame {
                        decoder.reinit().map_err(|_| failure("returned invalid concatenated zstd data."))?;
                    }
                    let progress = decoder.run_on_buffers(&chunk[offset..], &mut output)
                        .map_err(|_| failure("returned invalid zstd data."))?;
                    offset += progress.bytes_read;
                    finished_frame = progress.remaining == 0;
                    decoded = decoded.checked_add(progress.bytes_written)
                        .filter(|size| *size <= limit)
                        .ok_or_else(|| failure("returned an oversized decoded response."))?;
                    if progress.bytes_written > 0 {
                        yield Bytes::copy_from_slice(&output[..progress.bytes_written]);
                    }
                    if progress.bytes_read == 0 && progress.bytes_written == 0 {
                        if offset < chunk.len() {
                            Err(failure("returned a stalled zstd frame."))?;
                        }
                        break;
                    }
                    drain = !finished_frame && progress.bytes_written == output.len();
                }
            } else {
                for fragment in chunk.chunks(DECODE_CHUNK) {
                    context.check()?;
                    yield Bytes::copy_from_slice(fragment);
                }
            }
        }
        if decoder.is_some() && !finished_frame {
            Err(failure("closed before the zstd frame completed."))?;
        }
        context.check()?;
    }))
}

pub(crate) async fn read_json(
    response: Response,
    context: &RequestContext,
    limit: usize,
    operation: &'static str,
) -> Result<Value> {
    let status = error_status(response.status().as_u16());
    let mut stream = body(response, context.clone(), limit, operation)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
    }
    adapter_protocol::json::decode(&bytes, limit).map_err(|_| {
        AdapterError::new(
            status,
            "upstream_error",
            format!("{operation} returned a malformed JSON response."),
        )
    })
}

pub(crate) fn encode(value: &Value, limit: usize) -> Result<Vec<u8>> {
    let mut pending = vec![(value, 0)];
    while let Some((value, depth)) = pending.pop() {
        match value {
            Value::Array(values) => {
                if depth >= adapter_protocol::json::MAX_JSON_DEPTH {
                    return Err(AdapterError::invalid(
                        "The request exceeds the JSON nesting limit.",
                    ));
                }
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                if depth >= adapter_protocol::json::MAX_JSON_DEPTH {
                    return Err(AdapterError::invalid(
                        "The request exceeds the JSON nesting limit.",
                    ));
                }
                pending.extend(values.values().map(|value| (value, depth + 1)));
            }
            Value::Number(number) => {
                let spelling = number.to_string();
                if spelling.len() > adapter_protocol::json::MAX_NUMBER_BYTES
                    || spelling.parse::<serde_json::Number>().is_err()
                {
                    return Err(AdapterError::invalid(
                        "The request contains an invalid or oversized JSON number.",
                    ));
                }
            }
            _ => {}
        }
    }
    struct Limited {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(io::Error::other("request limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Limited {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    if serde_json::to_writer(&mut output, value).is_err() {
        return Err(if output.exceeded {
            AdapterError::new(
                413,
                "request_too_large",
                "The expanded request exceeds the 64 MiB limit.",
            )
        } else {
            AdapterError::invalid("The request must contain valid JSON values.")
        });
    }
    Ok(output.bytes)
}

pub(crate) fn request_id() -> String {
    static REQUESTS: AtomicU64 = AtomicU64::new(0);
    let sequence = REQUESTS.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        std::process::id() ^ (sequence >> 48) as u32,
        (time.as_secs() >> 12) & 0xffff,
        time.as_secs() & 0xfff,
        time.subsec_nanos() & 0xfff,
        sequence & 0xffffffffffff,
    )
}
