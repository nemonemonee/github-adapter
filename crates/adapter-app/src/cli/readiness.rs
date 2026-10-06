//! Strict bounded local HTTP framing and adapter identity verification.

use std::net::SocketAddr;
use std::time::Duration;

use adapter_protocol::{AdapterError, Result, Value, json};
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::{AccountScope, Provider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::discovery::{provider_name, same_upstream};

pub(super) const READINESS_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const HEALTH_BYTES: usize = 64 * 1024;
pub(super) const MODEL_BYTES: usize = 4 * 1024 * 1024;

pub fn endpoint(address: SocketAddr) -> String {
    format!("http://{address}")
}

pub(super) fn local_error(message: &'static str) -> AdapterError {
    AdapterError::new(503, "local_health_unavailable", message)
}

fn response_body(bytes: &[u8], eof: bool, limit: usize) -> Result<Option<Vec<u8>>> {
    let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return if eof || bytes.len() > 16 * 1024 {
            Err(local_error("The local HTTP headers are invalid."))
        } else {
            Ok(None)
        };
    };
    if end > 16 * 1024 {
        return Err(local_error("The local HTTP headers exceed their limit."));
    }
    let headers = std::str::from_utf8(&bytes[..end])
        .map_err(|_| local_error("The local HTTP headers are invalid."))?;
    let mut lines = headers.split("\r\n");
    let mut status = lines.next().unwrap_or("").split_whitespace();
    if !matches!(status.next(), Some("HTTP/1.0" | "HTTP/1.1")) || status.next() != Some("200") {
        return Err(local_error(
            "The local HTTP endpoint did not return success.",
        ));
    }
    let mut length = None;
    let mut chunked = false;
    let mut json_type = false;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| local_error("The local HTTP headers are invalid."))?;
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-type" => {
                json_type = value
                    .split(';')
                    .next()
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
            }
            "content-length" => {
                if length.is_some()
                    || value.is_empty()
                    || !value.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(local_error("The local HTTP length is invalid."));
                }
                length = Some(
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|length| *length <= limit)
                        .ok_or_else(|| local_error("The local HTTP response exceeds its limit."))?,
                );
            }
            "transfer-encoding" => {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(local_error(
                        "The local HTTP transfer encoding is unsupported.",
                    ));
                }
                chunked = true;
            }
            "content-encoding" if !value.eq_ignore_ascii_case("identity") => {
                return Err(local_error(
                    "The local HTTP content encoding is unsupported.",
                ));
            }
            _ => {}
        }
    }
    if !json_type || (chunked && length.is_some()) {
        return Err(local_error(
            "The local endpoint did not return unambiguous JSON framing.",
        ));
    }
    let body = &bytes[end + 4..];
    if let Some(length) = length {
        if body.len() < length {
            return if eof {
                Err(local_error("The local HTTP response was truncated."))
            } else {
                Ok(None)
            };
        }
        if body.len() != length {
            return Err(local_error(
                "Unexpected bytes followed the local HTTP response.",
            ));
        }
        return Ok(Some(body.to_vec()));
    }
    if chunked {
        let mut position = 0usize;
        let mut result = Vec::new();
        loop {
            let Some(end) = body[position..]
                .windows(2)
                .position(|window| window == b"\r\n")
            else {
                return if eof {
                    Err(local_error("The local chunked response was truncated."))
                } else {
                    Ok(None)
                };
            };
            let line = std::str::from_utf8(&body[position..position + end])
                .map_err(|_| local_error("The local chunked response is invalid."))?;
            let size = line.split(';').next().unwrap_or("");
            if size.is_empty()
                || size.len() > 16
                || !size.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(local_error("The local chunk size is invalid."));
            }
            let size = usize::from_str_radix(size, 16)
                .ok()
                .filter(|size| *size <= limit.saturating_sub(result.len()))
                .ok_or_else(|| local_error("The local HTTP response exceeds its limit."))?;
            position += end + 2;
            if size == 0 {
                let trailer = &body[position..];
                let end = if trailer.starts_with(b"\r\n") {
                    Some(2)
                } else {
                    trailer
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|end| end + 4)
                };
                if let Some(end) = end {
                    if end != trailer.len() {
                        return Err(local_error(
                            "Unexpected bytes followed the local HTTP response.",
                        ));
                    }
                    return Ok(Some(result));
                }
                return if eof {
                    Err(local_error("The local chunked response was truncated."))
                } else {
                    Ok(None)
                };
            }
            let end = position
                .checked_add(size)
                .filter(|end| end.checked_add(2).is_some_and(|end| end <= body.len()));
            let Some(end) = end else {
                return if eof {
                    Err(local_error("The local chunked response was truncated."))
                } else {
                    Ok(None)
                };
            };
            if &body[end..end + 2] != b"\r\n" {
                return Err(local_error("The local chunked response is invalid."));
            }
            result.extend_from_slice(&body[position..end]);
            position = end + 2;
        }
    }
    if body.len() > limit {
        return Err(local_error("The local HTTP response exceeds its limit."));
    }
    Ok(eof.then(|| body.to_vec()))
}

pub(super) async fn local_json(
    address: SocketAddr,
    path: &str,
    limit: usize,
    context: &RequestContext,
) -> Result<Value> {
    if !address.ip().is_loopback() || path.chars().any(char::is_control) {
        return Err(AdapterError::invalid(
            "Local readiness requests must use loopback.",
        ));
    }
    context.bounded(async {
        let mut stream = TcpStream::connect(address).await
            .map_err(|_| local_error("The local listener could not be reached."))?;
        stream.write_all(format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nAccept: application/json\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        ).as_bytes()).await.map_err(|_| local_error("Could not send the local readiness request."))?;
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await.map_err(|_| local_error("The local readiness connection failed."))?;
            if bytes.len().checked_add(count).is_none_or(|length| length > limit + 32 * 1024) {
                return Err(local_error("The local HTTP response exceeds its limit."));
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(body) = response_body(&bytes, count == 0, limit)? {
                return json::decode(&body, limit).map_err(|_| local_error("The local endpoint returned invalid JSON."));
            }
        }
    }).await
}

pub(super) fn recognized_health(value: Value) -> Result<Value> {
    if value.get("application").and_then(Value::as_str) != Some("github-adapter")
        || !matches!(
            value.get("provider").and_then(Value::as_str),
            Some("mai" | "github")
        )
    {
        return Err(local_error(
            "The port is not reporting a recognized GitHub Adapter.",
        ));
    }
    Ok(value)
}

pub(super) fn matching_health(health: Value, scope: &AccountScope) -> Result<()> {
    let health = recognized_health(health)?;
    if health.get("status").and_then(Value::as_str) != Some("ok")
        || health.get("engine").and_then(Value::as_str) != Some("rust")
        || health.get("provider").and_then(Value::as_str) != Some(provider_name(scope.provider))
        || (scope.provider == Provider::Github
            && !health
                .get("account")
                .and_then(Value::as_str)
                .is_some_and(|account| account.eq_ignore_ascii_case(&scope.account)))
        || (scope.provider == Provider::Mai
            && !health
                .get("upstream")
                .and_then(Value::as_str)
                .is_some_and(|upstream| same_upstream(upstream, &scope.account)))
    {
        return Err(local_error(
            "Local readiness does not match the pinned native provider/account/upstream. No app was launched.",
        ));
    }
    Ok(())
}
