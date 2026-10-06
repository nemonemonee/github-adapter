//! Opt-in, single-use encrypted-state recovery. Never replay visible output.
use super::*;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const PRELUDE_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(super) struct Trace {
    id: u64,
    visible: Arc<AtomicBool>,
    bytes: Arc<AtomicUsize>,
    request_id: Arc<StdMutex<Option<String>>>,
}

impl Trace {
    fn new(id: u64) -> Self {
        Self {
            id,
            visible: Arc::new(AtomicBool::new(false)),
            bytes: Arc::new(AtomicUsize::new(0)),
            request_id: Arc::new(StdMutex::new(None)),
        }
    }
    pub(super) fn set_request_id(
        &self,
        headers: &reqwest::header::HeaderMap,
        redaction: &Redaction,
    ) {
        let value = ["x-request-id", "x-github-request-id", "request-id"]
            .into_iter()
            .find_map(|key| {
                headers
                    .get(key)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| identifier(v, redaction))
            });
        if let Ok(mut id) = self.request_id.lock() {
            *id = value;
        }
    }
    pub(super) fn observe_json(&self, value: &Value) {
        if has_output(value) {
            self.visible.store(true, Ordering::Relaxed);
        }
    }
    fn eligible(&self) -> bool {
        !self.visible.load(Ordering::Relaxed) && self.bytes.load(Ordering::Relaxed) <= PRELUDE_BYTES
    }
}

#[derive(Default)]
pub(super) struct Recovery {
    enabled: AtomicBool,
    sequence: AtomicU64,
    failures: StdMutex<VecDeque<(u64, Value)>>,
}

fn identifier(value: &str, redaction: &Redaction) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_:-".contains(&b))
        && redaction.text(value) == value)
        .then(|| value.to_owned())
}

fn encrypted_error(message: &str) -> bool {
    let s = message.to_ascii_lowercase();
    (s.contains("encrypted content")
        && s.contains("could not be verified")
        && (s.contains("could not be decrypted") || s.contains("could not be parsed")))
        || (s.contains("encrypted function output content")
            && s.contains("could not be decrypted or decoded"))
}

fn retryable(error: &AdapterError) -> bool {
    matches!(error.status, 400 | 422 | 500 | 502) && encrypted_error(&error.message)
}

fn has_output(value: &Value) -> bool {
    let observed = |value: &Value| match value.get("output") {
        None | Some(Value::Null) => false,
        Some(Value::Array(output)) => !output.is_empty(),
        // Malformed output is not evidence that nothing was generated.
        Some(_) => true,
    };
    observed(value) || value.get("response").is_some_and(observed)
}

fn lifecycle(event: &SseEvent) -> bool {
    let output = event.data.as_ref().is_some_and(has_output);
    !output
        && (event.comment.is_some()
            || matches!(
                event.kind.as_str(),
                "response.created" | "response.in_progress"
            ))
}

impl Recovery {
    pub(super) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    pub(super) fn trace(&self) -> Trace {
        Trace::new(self.sequence.fetch_add(1, Ordering::Relaxed))
    }
    pub(super) fn snapshot(&self) -> Value {
        self.failures
            .lock()
            .map(|entries| Value::Array(entries.iter().map(|(_, value)| value.clone()).collect()))
            .unwrap_or_else(|_| json!([]))
    }
    pub(super) fn observe_failure(
        &self,
        prepared: &Prepared,
        event: &str,
        value: &Value,
        redaction: &Redaction,
    ) {
        // All failure transports must mark observed output before error conversion.
        prepared.trace.observe_json(value);
        let resource = value.get("response").unwrap_or(value);
        let error = resource.get("error").unwrap_or(resource);
        // Never retain upstream prose: it can contain prompts, credentials or ciphertext.
        let code = error["code"]
            .as_str()
            .or(error["type"].as_str())
            .filter(|code| {
                matches!(
                    *code,
                    "invalid_encrypted_content"
                        | "invalid_request_error"
                        | "server_error"
                        | "rate_limit_error"
                        | "context_length_exceeded"
                )
            })
            .unwrap_or("upstream_error");
        let value = json!({
            "event": event, "model": prepared.route.model, "error_code": code,
            "reason": if encrypted_error(error["message"].as_str().unwrap_or("")) { "encrypted_state_rejected" } else { "upstream_response_failed" },
            "response_id": resource["id"].as_str().and_then(|v| identifier(v, redaction)),
            "request_id": prepared.trace.request_id.lock().ok().and_then(|id| id.clone()),
            "recovery": if self.enabled() { "not_attempted" } else { "disabled" },
        });
        if let Ok(mut entries) = self.failures.lock() {
            if let Some((_, previous)) = entries.iter_mut().find(|(id, _)| *id == prepared.trace.id)
            {
                let outcome = previous["recovery"].clone();
                *previous = value;
                previous["recovery"] = outcome;
            } else {
                if entries.len() == 10 {
                    entries.pop_front();
                }
                entries.push_back((prepared.trace.id, value));
            }
        }
    }
    pub(super) fn observe(&self, prepared: &Prepared, events: &[SseEvent], bytes: usize) {
        if !prepared.trace.visible.load(Ordering::Relaxed) {
            prepared.trace.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
        for event in events {
            if let Some(value) = &event.data {
                // Inspect raw output before native validation can turn a failure
                // into an error and discard its response-resource envelope.
                prepared.trace.observe_json(value);
            }
            if matches!(event.kind.as_str(), "response.failed" | "error") {
                if let Some(value) = &event.data {
                    self.observe_failure(
                        prepared,
                        &event.kind,
                        value,
                        &prepared.binding.redaction(),
                    );
                }
            } else if !lifecycle(event) && !event.done {
                prepared.trace.visible.store(true, Ordering::Relaxed);
            }
        }
    }
    fn outcome(&self, id: u64, outcome: &str) {
        if let Ok(mut entries) = self.failures.lock()
            && let Some((_, value)) = entries.iter_mut().find(|(key, _)| *key == id)
        {
            value["recovery"] = json!(outcome);
        }
    }
    fn candidate(&self, prepared: &Prepared) -> Option<Prepared> {
        if !self.enabled() || !prepared.binding.github() || !prepared.native {
            return None;
        }
        let input = prepared.request["input"].as_array()?;
        // A compaction item can be the sole surviving context. Never discard it,
        // or any unrecognized opaque state, just to obtain a successful retry.
        if input.iter().any(|item| {
            item["type"] == "compaction"
                || (item.get("encrypted_content").is_some()
                    && !matches!(
                        item["type"].as_str(),
                        Some("reasoning" | "function_call_output")
                    ))
        }) {
            return None;
        }
        let mut changed = false;
        let mut sanitized = Vec::with_capacity(input.len());
        for item in input {
            if item["type"] == "reasoning" && item.get("encrypted_content").is_some() {
                changed = true;
                continue;
            }
            let mut item = item.clone();
            if item["type"] == "function_call_output" && item.get("encrypted_content").is_some() {
                if !(item["output"].is_string() || item["output"].is_array()) {
                    return None;
                }
                item.as_object_mut()?.remove("encrypted_content");
                changed = true;
            }
            sanitized.push(item);
        }
        if !changed || sanitized.is_empty() {
            return None;
        }
        let mut next = prepared.clone();
        next.request["input"] = json!(sanitized);
        next.outgoing = next.request.clone();
        next.trace = Trace::new(prepared.trace.id);
        Some(next)
    }
}

impl Backend {
    pub fn set_encrypted_state_recovery(&self, enabled: bool) {
        self.recovery.enabled.store(enabled, Ordering::Relaxed);
    }

    pub async fn complete_responses(&self, payload: Value, ctx: RequestContext) -> Result<Value> {
        let _guard = ctx.cancel_on_drop();
        let prepared = self
            .prepare(&payload, RequestKind::Responses { stream: false }, &ctx)
            .await?;
        let Some(next) = self.recovery.candidate(&prepared) else {
            return self.complete_prepared(payload, prepared, ctx.child()).await;
        };
        let id = prepared.trace.id;
        let result = self
            .complete_prepared(payload.clone(), prepared.clone(), ctx.child())
            .await;
        if let Err(error) = &result
            && prepared.trace.eligible()
            && retryable(error)
        {
            self.recovery.outcome(id, "retrying");
            let result = self.complete_prepared(payload, next, ctx.child()).await;
            self.recovery.outcome(
                id,
                if result.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                },
            );
            return result;
        }
        result
    }

    pub async fn stream_responses(
        self: Arc<Self>,
        payload: Value,
        ctx: RequestContext,
    ) -> Result<EventStream> {
        let guard = ctx.cancel_on_drop();
        let prepared = self
            .prepare(&payload, RequestKind::Responses { stream: true }, &ctx)
            .await?;
        let Some(next) = self.recovery.candidate(&prepared) else {
            // Transfer the caller's cancellation lifetime into the returned stream.
            let stream = self.stream_prepared(payload, prepared, ctx.child()).await?;
            return Ok(Box::pin(async_stream::try_stream! {
                let _guard = guard;
                let mut stream = stream;
                while let Some(event) = stream.next().await { yield event?; }
            }));
        };
        let id = prepared.trace.id;
        let first = self
            .clone()
            .stream_prepared(payload.clone(), prepared.clone(), ctx.child())
            .await;
        let mut retried = false;
        let mut next = Some(next);
        let stream = match first {
            Ok(stream) => stream,
            Err(error) if prepared.trace.eligible() && retryable(&error) => {
                retried = true;
                self.recovery.outcome(id, "retrying");
                match self
                    .clone()
                    .stream_prepared(payload.clone(), next.take().unwrap(), ctx.child())
                    .await
                {
                    Ok(stream) => stream,
                    Err(error) => {
                        self.recovery.outcome(id, "failed");
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        };
        Ok(Box::pin(async_stream::try_stream! {
            let _guard = guard;
            let mut stream = stream;
            let mut held = Vec::new();
            let mut held_bytes = 0usize;
            let mut emitted = false;
            loop {
                match stream.next().await {
                    Some(Ok(event)) => {
                        if !retried && !emitted && lifecycle(&event) {
                            let length = event.to_bytes()?.len();
                            if held_bytes.saturating_add(length) <= PRELUDE_BYTES {
                                held_bytes += length; held.push(event); continue;
                            }
                        }
                        emitted = true;
                        for event in held.drain(..) { yield event; }
                        if retried && is_terminal(&event) { self.recovery.outcome(id, "succeeded"); }
                        yield event;
                    }
                    Some(Err(error)) if !retried && !emitted && prepared.trace.eligible() && retryable(&error) => {
                        held.clear();
                        retried = true;
                        self.recovery.outcome(id, "retrying");
                        // The failed attempt owns a child cancellation token. Dropping
                        // it must not cancel the parent's remaining recovery budget.
                        drop(stream);
                        stream = match self.clone().stream_prepared(payload.clone(), next.take().unwrap(), ctx.child()).await {
                            Ok(stream) => stream,
                            Err(error) => { self.recovery.outcome(id, "failed"); Err(error)?; unreachable!() }
                        };
                    }
                    Some(Err(error)) => {
                        if retried { self.recovery.outcome(id, "failed"); }
                        for event in held.drain(..) { yield event; }
                        Err(error)?;
                    }
                    None => { for event in held { yield event; } break; }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_empty_lifecycle_resources_can_be_held_as_recoverable_prelude() {
        for kind in ["response.created", "response.in_progress"] {
            assert!(lifecycle(&SseEvent::json(
                kind,
                json!({"type":kind,"response":{"output":[]}})
            )));
            for output in [
                json!([{"type":"message"}]),
                json!({"unexpected":"partial"}),
                json!("partial"),
            ] {
                let event = SseEvent::json(kind, json!({"type":kind,"response":{"output":output}}));
                assert!(!lifecycle(&event));
                let trace = Trace::new(1);
                trace.observe_json(&event.data.as_ref().unwrap()["response"]);
                assert!(!trace.eligible());
            }
        }
    }
}
