//! Protocol-specific stream and resource validation, independent of backend state.

use crate::transport::{self, Redaction};
use adapter_protocol::chat_stream::ChatResponseStream;
use adapter_protocol::identity::NativeStream;
use adapter_protocol::sse::SseEvent;
use adapter_protocol::{AdapterError, Result, Value};
use futures_util::{Stream, StreamExt};
use reqwest::Response;
use reqwest::header::CONTENT_ENCODING;
use std::collections::BTreeSet;
use std::pin::Pin;

pub type EventStream = Pin<Box<dyn Stream<Item = Result<SseEvent>> + Send + 'static>>;

#[derive(Default)]
pub(super) struct OutputBudget {
    pub(super) bytes: usize,
}

impl OutputBudget {
    pub(super) fn add(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|bytes| *bytes <= adapter_protocol::sse::MAX_SSE_STREAM_BYTES)
            .ok_or_else(|| {
                AdapterError::upstream("The outgoing SSE stream exceeds the byte limit.")
            })?;
        Ok(())
    }

    pub(super) fn event(&mut self, event: &SseEvent) -> Result<()> {
        self.add(event.to_bytes()?.len())
    }
}

pub(super) fn is_terminal(event: &SseEvent) -> bool {
    matches!(
        event.kind.as_str(),
        "response.completed" | "response.incomplete"
    )
}

pub(super) fn terminal_kind(response: &Value) -> Result<String> {
    match response.get("status").and_then(Value::as_str) {
        Some(status @ ("completed" | "incomplete")) => Ok(format!("response.{status}")),
        _ => Err(AdapterError::upstream(
            "The native response did not contain a terminal status.",
        )),
    }
}

pub(super) fn terminal_value(event: &SseEvent) -> Result<Value> {
    event
        .data
        .as_ref()
        .and_then(|data| data.get("response"))
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| AdapterError::upstream("The upstream omitted its Responses resource."))
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ResourceUse {
    Inference,
    Background,
    Retrieval,
}

pub(super) fn resource_value(value: Value, usage: ResourceUse) -> Result<Value> {
    if !value
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        return Err(AdapterError::upstream(
            "A published response resource must contain an identifier.",
        ));
    }
    let terminal = matches!(value["status"].as_str(), Some("completed" | "incomplete"));
    let kind = if terminal {
        terminal_kind(&value)?
    } else {
        match value["status"].as_str() {
            Some("queued" | "in_progress") if usage != ResourceUse::Inference => {}
            Some("failed" | "cancelled") if usage == ResourceUse::Retrieval => {}
            _ => {
                return Err(AdapterError::upstream(
                    "The upstream returned an invalid response resource status.",
                ));
            }
        }
        // Retrieved failures are snapshots, not new failed inference events.
        "response.in_progress".to_owned()
    };
    let mut state = NativeStream::new();
    let event = state.push(SseEvent::json(
        &kind,
        serde_json::json!({"type":kind, "response":value}),
    ))?;
    if terminal {
        state.finish()?;
    }
    terminal_value(&event)
}

pub(super) fn compressed(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("zstd"))
}

pub(super) fn redacted_stream(stream: EventStream, redaction: Redaction) -> EventStream {
    Box::pin(stream.map(move |event| event.map_err(|error| redaction.error(error))))
}

pub(super) enum Mode {
    Native {
        state: Box<NativeStream>,
        terminal: bool,
    },
    Bridge {
        state: Box<ChatResponseStream>,
        done: bool,
    },
    Direct(DirectChat),
}

impl Mode {
    pub(super) fn native() -> Self {
        Self::Native {
            state: Box::default(),
            terminal: false,
        }
    }
    pub(super) fn bridge(request: &Value) -> Result<Self> {
        Ok(Self::Bridge {
            state: Box::new(ChatResponseStream::new(request, None)?),
            done: false,
        })
    }
    pub(super) fn direct(&self) -> bool {
        matches!(self, Self::Direct(_))
    }
    pub(super) fn complete(&self) -> bool {
        match self {
            Self::Native { terminal, .. } => *terminal,
            Self::Bridge { done, .. } => *done,
            Self::Direct(state) => state.done,
        }
    }
    pub(super) fn start(&mut self) -> Result<Vec<SseEvent>> {
        match self {
            Self::Bridge { state, .. } => state.start(),
            _ => Ok(Vec::new()),
        }
    }
    pub(super) fn batch(&mut self, events: Vec<SseEvent>) -> Result<Vec<SseEvent>> {
        let mut result = Vec::new();
        for event in events {
            match self {
                Self::Native { state, terminal } => {
                    let event = state.push(event)?;
                    if event.done || (*terminal && event.comment.is_some()) {
                        continue;
                    }
                    if is_terminal(&event) {
                        *terminal = true;
                    }
                    result.push(event);
                }
                Self::Bridge { state, done } => {
                    if event.comment.is_some() {
                        if !*done {
                            result.push(event);
                        }
                    } else if *done {
                        return Err(AdapterError::upstream(
                            "The Chat stream sent data after [DONE].",
                        ));
                    } else if event.done {
                        result.extend(state.finish()?);
                        *done = true;
                    } else if let Some(data) = event.data {
                        result.extend(state.feed(&data)?);
                    }
                }
                Self::Direct(state) => {
                    if state.accept(&event)? {
                        result.push(event);
                    }
                }
            }
        }
        Ok(result)
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        match self {
            Self::Native { state, .. } => state.finish(),
            Self::Bridge { done: true, .. } => Ok(()),
            Self::Direct(state) if state.done => Ok(()),
            _ => Err(AdapterError::upstream(
                "The Chat stream closed before [DONE].",
            )),
        }
    }
}

#[derive(Default)]
pub(super) struct DirectChat {
    id: Option<String>,
    seen: BTreeSet<u64>,
    finished: BTreeSet<u64>,
    done: bool,
}

impl DirectChat {
    fn accept(&mut self, event: &SseEvent) -> Result<bool> {
        if event.comment.is_some() {
            return Ok(!self.done);
        }
        if self.done {
            return Err(AdapterError::upstream(
                "The Chat stream sent data after [DONE].",
            ));
        }
        if event.done {
            if self.seen.is_empty() || self.seen != self.finished {
                return Err(AdapterError::upstream(
                    "The Chat stream ended before its finish reasons.",
                ));
            }
            self.done = true;
            return Ok(true);
        }
        let data = event
            .data
            .as_ref()
            .filter(|value| value.is_object())
            .ok_or_else(|| {
                AdapterError::upstream("The upstream returned an invalid Chat chunk.")
            })?;
        if event.kind == "error" || data.get("error").is_some_and(|error| !error.is_null()) {
            return Err(AdapterError::upstream(transport::message(
                data,
                "The Chat stream failed.",
            )));
        }
        if data
            .get("object")
            .is_some_and(|value| value != "chat.completion.chunk")
        {
            return Err(AdapterError::upstream(
                "The upstream returned an invalid Chat chunk object.",
            ));
        }
        if let Some(id) = data.get("id").filter(|value| !value.is_null()) {
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AdapterError::upstream("The Chat chunk ID is invalid."))?;
            if self.id.as_deref().is_some_and(|previous| previous != id) {
                return Err(AdapterError::upstream(
                    "The Chat stream changed its completion ID.",
                ));
            }
            self.id = Some(id.to_owned());
        }
        validate_direct_usage(data.get("usage"))?;
        let choices = data
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| AdapterError::upstream("The Chat chunk has invalid choices."))?;
        let mut present = BTreeSet::new();
        for choice in choices {
            let index = choice
                .get("index")
                .and_then(transport::unsigned)
                .ok_or_else(|| {
                    AdapterError::upstream("The Chat chunk has an invalid choice index.")
                })?;
            if !present.insert(index)
                || self.finished.contains(&index)
                || !choice.get("delta").is_some_and(Value::is_object)
            {
                return Err(AdapterError::upstream(
                    "The Chat chunk has an invalid or already finished choice.",
                ));
            }
            self.seen.insert(index);
            if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
                if !reason.as_str().is_some_and(|reason| !reason.is_empty()) {
                    return Err(AdapterError::upstream("The Chat finish reason is invalid."));
                }
                self.finished.insert(index);
            }
        }
        Ok(true)
    }
}

fn validate_direct_usage(usage: Option<&Value>) -> Result<()> {
    let Some(usage) = usage.filter(|usage| !usage.is_null()) else {
        return Ok(());
    };
    let object = usage
        .as_object()
        .ok_or_else(|| AdapterError::upstream("The Chat usage must be an object."))?;
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "reasoning_tokens",
    ] {
        if object
            .get(key)
            .is_some_and(|value| !value.is_null() && transport::unsigned(value).is_none())
        {
            return Err(AdapterError::upstream(
                "The Chat usage contains invalid token counts.",
            ));
        }
    }
    if object
        .get("prompt_tokens")
        .is_some_and(|value| !value.is_null())
        && object
            .get("completion_tokens")
            .is_some_and(|value| !value.is_null())
    {
        adapter_protocol::usage::convert_usage(usage)
            .map_err(|error| AdapterError::upstream(error.message))?;
    }
    Ok(())
}

pub(super) fn validate_chat_completion(value: &Value) -> Result<()> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(AdapterError::upstream(transport::message(
            value,
            "The Chat completion failed.",
        )));
    }
    if value
        .get("object")
        .is_some_and(|object| object != "chat.completion")
    {
        return Err(AdapterError::upstream(
            "The upstream returned an invalid Chat completion object.",
        ));
    }
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| !choices.is_empty())
        .ok_or_else(|| {
            AdapterError::upstream("The upstream returned invalid Chat completion choices.")
        })?;
    let mut seen = BTreeSet::new();
    for (default, choice) in choices.iter().enumerate() {
        let index = match choice.get("index") {
            Some(index) => transport::unsigned(index).ok_or_else(|| {
                AdapterError::upstream("The Chat completion choice index is invalid.")
            })?,
            None => default as u64,
        };
        if !seen.insert(index)
            || !choice.get("message").is_some_and(Value::is_object)
            || !choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| !reason.is_empty())
        {
            return Err(AdapterError::upstream(
                "The upstream returned an invalid Chat completion choice.",
            ));
        }
    }
    validate_direct_usage(value.get("usage"))
}
