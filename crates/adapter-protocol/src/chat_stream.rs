//! Incremental, single-choice Chat -> Responses translation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::chat::{
    array, as_upstream, completion_parts, emit, event_items, field, fields, finite_json, neutral,
    nonblank, object, one_of, request_options, response_resource, stable_id, string, uint,
};
use crate::error::{AdapterError, Result};
use crate::sse::SseEvent;
use crate::usage::convert_usage;

static STREAM_IDS: AtomicU64 = AtomicU64::new(0);

/// The transport must call `finish` on `[DONE]`, never on EOF. A finish reason
/// alone does not complete this codec. Malformed chunks poison an active stream.
#[derive(Debug)]
pub struct ChatResponseStream {
    request: Value,
    response_id: String,
    created_at: u64,
    sequence: u64,
    started: bool,
    finished: bool,
    failed: bool,
    finish_reason: Option<String>,
    upstream_id: Option<String>,
    usage: Value,
    output: Vec<Value>,
    message_index: Option<usize>,
    tools: BTreeMap<u64, usize>,
    announced_tools: BTreeSet<usize>,
    response: Option<Value>,
}

impl ChatResponseStream {
    pub fn new(request: &Value, response_id: Option<&str>) -> Result<Self> {
        request_options(request)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
            AdapterError::new(
                500,
                "internal_error",
                "The local clock predates the Unix epoch",
            )
        })?;
        let response_id = match response_id {
            Some(id) if !id.trim().is_empty() => id.to_owned(),
            Some(_) => {
                return Err(AdapterError::invalid(
                    "response_id must be a nonempty string",
                ));
            }
            None => stable_id(
                "resp",
                &json!([
                    now.as_nanos().to_string(),
                    std::process::id(),
                    STREAM_IDS.fetch_add(1, Ordering::Relaxed),
                ]),
            )?,
        };
        Ok(Self {
            request: request.clone(),
            response_id,
            created_at: now.as_secs(),
            sequence: 0,
            started: false,
            finished: false,
            failed: false,
            finish_reason: None,
            upstream_id: None,
            usage: Value::Null,
            output: Vec::new(),
            message_index: None,
            tools: BTreeMap::new(),
            announced_tools: BTreeSet::new(),
            response: None,
        })
    }

    pub fn response(&self) -> Option<Value> {
        self.response.clone()
    }

    fn emit(&mut self, kind: &str, data: Value) -> Result<SseEvent> {
        emit(&mut self.sequence, kind, data)
    }

    pub fn start(&mut self) -> Result<Vec<SseEvent>> {
        if self.started {
            return Err(AdapterError::upstream(
                "The Chat stream has already started",
            ));
        }
        let initial = response_resource(
            &self.request,
            &self.response_id,
            self.created_at,
            "in_progress",
            Vec::new(),
            Value::Null,
            None,
        )?;
        let events = vec![
            self.emit("response.created", json!({"response": initial}))?,
            self.emit("response.in_progress", json!({"response": initial}))?,
        ];
        self.started = true;
        Ok(events)
    }

    fn append(item: &mut Value, key: &str, fragment: &str) -> Result<()> {
        match item.get_mut(key) {
            Some(Value::String(text)) => {
                text.push_str(fragment);
                Ok(())
            }
            _ => Err(AdapterError::upstream(
                "Invalid internal Chat accumulator field",
            )),
        }
    }

    fn content_delta(&mut self, part: &Value, events: &mut Vec<SseEvent>) -> Result<()> {
        let message_index = match self.message_index {
            Some(index) => index,
            None => {
                let index = self.output.len();
                let item = json!({
                    "id": stable_id("msg", &json!([self.response_id, index]))?,
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [],
                });
                self.output.push(item.clone());
                self.message_index = Some(index);
                events.push(self.emit(
                    "response.output_item.added",
                    json!({
                        "output_index": index, "item": item,
                    }),
                )?);
                index
            }
        };
        let text = part["type"] == "output_text";
        let field = if text { "text" } else { "refusal" };
        let fragment = string(&part[field], "stream.content")?;
        let (item_id, content_index, added) = {
            let item = &mut self.output[message_index];
            let item_id = item["id"].clone();
            let content = item["content"]
                .as_array_mut()
                .ok_or_else(|| AdapterError::upstream("Invalid internal Chat message content"))?;
            let mut added = None;
            if content
                .last()
                .is_none_or(|last| last["type"] != part["type"])
            {
                let mut empty = part.clone();
                empty[field] = json!("");
                content.push(empty.clone());
                added = Some(empty);
            }
            let content_index = content.len() - 1;
            Self::append(&mut content[content_index], field, fragment)?;
            (item_id, content_index, added)
        };
        if let Some(part) = added {
            events.push(self.emit(
                "response.content_part.added",
                json!({
                    "output_index": message_index, "item_id": item_id,
                    "content_index": content_index, "part": part,
                }),
            )?);
        }
        let mut delta = json!({
            "output_index": message_index, "item_id": item_id,
            "content_index": content_index, "delta": fragment,
        });
        if text {
            delta["logprobs"] = json!([]);
        }
        events.push(self.emit(
            if text {
                "response.output_text.delta"
            } else {
                "response.refusal.delta"
            },
            delta,
        )?);
        Ok(())
    }

    fn announce_tool(&mut self, index: usize, events: &mut Vec<SseEvent>) -> Result<()> {
        if self.announced_tools.contains(&index) {
            return Ok(());
        }
        let item = self.output[index].clone();
        nonblank(&item["call_id"], "stream.tool_call.id")?;
        nonblank(&item["name"], "stream.tool_call.name")?;
        let mut initial = item.clone();
        initial["status"] = json!("in_progress");
        initial["arguments"] = json!("");
        events.push(self.emit(
            "response.output_item.added",
            json!({
                "output_index": index, "item": initial,
            }),
        )?);
        if !string(&item["arguments"], "stream.tool_call.arguments")?.is_empty() {
            events.push(self.emit(
                "response.function_call_arguments.delta",
                json!({
                    "output_index": index, "item_id": item["id"], "delta": item["arguments"],
                }),
            )?);
        }
        self.announced_tools.insert(index);
        Ok(())
    }

    fn tool_delta(&mut self, raw: &Value, events: &mut Vec<SseEvent>) -> Result<()> {
        let call = object(raw, "stream.tool_call")?;
        fields(
            call,
            &["index", "id", "type", "function"],
            "stream.tool_call",
        )?;
        let index = uint(field(call, "index"), "stream.tool_call.index", 0)?;
        if !field(call, "type").is_null() && field(call, "type") != "function" {
            return Err(AdapterError::invalid(
                "Only function tool deltas are representable",
            ));
        }
        let empty = json!({});
        let function = object(
            call.get("function").unwrap_or(&empty),
            "stream.tool_call.function",
        )?;
        fields(
            function,
            &["name", "arguments"],
            "stream.tool_call.function",
        )?;
        let output_index = match self.tools.get(&index) {
            Some(index) => *index,
            None => {
                let output_index = self.output.len();
                self.output.push(json!({
                    "id": stable_id("fc", &json!([self.response_id, output_index]))?,
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": "",
                    "name": "",
                    "arguments": "",
                }));
                self.tools.insert(index, output_index);
                output_index
            }
        };
        let announced = self.announced_tools.contains(&output_index);
        for (key, value) in [
            ("call_id", field(call, "id")),
            ("name", field(function, "name")),
        ] {
            if value.is_null() {
                continue;
            }
            let fragment = string(value, &format!("stream.tool_call.{key}"))?;
            if announced {
                if !fragment.is_empty() && self.output[output_index][key] != fragment {
                    return Err(AdapterError::invalid(
                        "A tool identity changed after its arguments started",
                    ));
                }
            } else {
                Self::append(&mut self.output[output_index], key, fragment)?;
            }
        }
        if !field(function, "arguments").is_null() {
            let fragment = string(field(function, "arguments"), "stream.tool_call.arguments")?;
            Self::append(&mut self.output[output_index], "arguments", fragment)?;
            if announced && !fragment.is_empty() {
                let item_id = self.output[output_index]["id"].clone();
                events.push(self.emit(
                    "response.function_call_arguments.delta",
                    json!({
                        "output_index": output_index, "item_id": item_id, "delta": fragment,
                    }),
                )?);
            }
        }
        let item = &self.output[output_index];
        if !announced
            && !string(&item["arguments"], "stream.tool_call.arguments")?.is_empty()
            && !string(&item["call_id"], "stream.tool_call.id")?.is_empty()
            && !string(&item["name"], "stream.tool_call.name")?.is_empty()
        {
            self.announce_tool(output_index, events)?;
        }
        Ok(())
    }

    pub fn feed(&mut self, chunk: &Value) -> Result<Vec<SseEvent>> {
        if !self.started || self.finished || self.failed {
            return Err(AdapterError::upstream(
                "Chat deltas require an active, unfinished stream",
            ));
        }
        match self.feed_inner(chunk) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(as_upstream(error))
            }
        }
    }

    fn feed_inner(&mut self, chunk: &Value) -> Result<Vec<SseEvent>> {
        finite_json(chunk, "stream.chunk")?;
        let chunk = object(chunk, "stream.chunk")?;
        if !field(chunk, "error").is_null() {
            return Err(AdapterError::invalid(
                "The Chat stream contains an upstream error",
            ));
        }
        if chunk
            .get("object")
            .is_some_and(|value| value != "chat.completion.chunk")
        {
            return Err(AdapterError::invalid("Expected a chat.completion.chunk"));
        }
        if !field(chunk, "id").is_null() {
            let id = nonblank(field(chunk, "id"), "stream.id")?;
            if self
                .upstream_id
                .as_deref()
                .is_some_and(|previous| previous != id)
            {
                return Err(AdapterError::invalid(
                    "The upstream Chat stream changed its response ID",
                ));
            }
            self.upstream_id = Some(id.to_owned());
        }
        let choices = array(field(chunk, "choices"), "stream.choices")?;
        if !field(chunk, "usage").is_null() {
            let usage = convert_usage(field(chunk, "usage"))?;
            let final_usage = choices.is_empty()
                || (choices.len() == 1
                    && choices[0]
                        .get("finish_reason")
                        .is_some_and(|reason| !reason.is_null()));
            if final_usage {
                if !self.usage.is_null() && self.usage != usage {
                    return Err(AdapterError::invalid(
                        "The Chat stream supplied conflicting final usage",
                    ));
                }
                self.usage = usage;
            }
        }
        if choices.is_empty() {
            if field(chunk, "usage").is_null() {
                return Err(AdapterError::invalid(
                    "An empty choices chunk must contain final usage",
                ));
            }
            return Ok(Vec::new());
        }
        if choices.len() != 1 {
            return Err(AdapterError::invalid(
                "The Chat stream must contain exactly one choice",
            ));
        }
        if self.finish_reason.is_some() {
            return Err(AdapterError::invalid(
                "The Chat stream sent a choice after its finish reason",
            ));
        }
        let choice = object(&choices[0], "stream.choice")?;
        if uint(field(choice, "index"), "stream.choice.index", 0)? != 0 {
            return Err(AdapterError::invalid(
                "The Chat stream choice index must be zero",
            ));
        }
        neutral(choice, "logprobs", &[Value::Null], "stream.choice")?;
        let finish_reason = if field(choice, "finish_reason").is_null() {
            None
        } else {
            Some(
                one_of(
                    field(choice, "finish_reason"),
                    &["stop", "tool_calls", "length", "content_filter"],
                    "stream.finish_reason",
                )?
                .to_owned(),
            )
        };
        let delta = field(choice, "delta");
        let parts = completion_parts(delta)?;
        let mut events = Vec::new();
        for part in parts {
            self.content_delta(&part, &mut events)?;
        }
        if !delta["tool_calls"].is_null() {
            for call in array(&delta["tool_calls"], "stream.delta.tool_calls")? {
                self.tool_delta(call, &mut events)?;
            }
        }
        self.finish_reason = finish_reason;
        Ok(events)
    }

    /// A successful call represents both an already observed finish reason and
    /// the transport's explicit `[DONE]`. Missing final usage remains null.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>> {
        if !self.started || self.finished || self.failed {
            return Err(AdapterError::upstream("The Chat stream is not active"));
        }
        if self.finish_reason.is_none() {
            return Err(AdapterError::upstream(
                "The Chat stream ended before a finish reason",
            ));
        }
        match self.finish_inner() {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(as_upstream(error))
            }
        }
    }

    fn finish_inner(&mut self) -> Result<Vec<SseEvent>> {
        let finish = self
            .finish_reason
            .as_deref()
            .ok_or_else(|| AdapterError::invalid("The Chat stream ended before a finish reason"))?;
        if finish == "tool_calls" && self.tools.is_empty() {
            return Err(AdapterError::invalid(
                "The Chat stream finished with tool_calls but supplied no calls",
            ));
        }
        if self
            .tools
            .keys()
            .enumerate()
            .any(|(expected, actual)| expected as u64 != *actual)
        {
            return Err(AdapterError::invalid(
                "The Chat stream supplied noncontiguous tool indexes",
            ));
        }
        let reason = match finish {
            "length" => Some("max_output_tokens"),
            "content_filter" => Some("content_filter"),
            _ => None,
        };
        let status = if reason.is_some() {
            "incomplete"
        } else {
            "completed"
        };
        let mut output = self.output.clone();
        for item in &mut output {
            item["status"] = json!(status);
        }
        let response = response_resource(
            &self.request,
            &self.response_id,
            self.created_at,
            status,
            output,
            self.usage.clone(),
            reason,
        )?;
        let items = event_items(&response)?;
        let mut events = Vec::new();
        for (output_index, item) in items.iter().enumerate() {
            let position = json!({"output_index": output_index, "item_id": item["id"]});
            if item["type"] == "function_call" {
                self.announce_tool(output_index, &mut events)?;
                let mut done = position;
                done["arguments"] = item["arguments"].clone();
                done["name"] = item["name"].clone();
                events.push(self.emit("response.function_call_arguments.done", done)?);
            } else {
                for (content_index, part) in array(&item["content"], "stream.content")?
                    .iter()
                    .enumerate()
                {
                    let text = part["type"] == "output_text";
                    let field = if text { "text" } else { "refusal" };
                    let mut position = position.clone();
                    position["content_index"] = json!(content_index);
                    let mut done = position.clone();
                    done[field] = part[field].clone();
                    if text {
                        done["logprobs"] = json!([]);
                    }
                    events.push(self.emit(
                        if text {
                            "response.output_text.done"
                        } else {
                            "response.refusal.done"
                        },
                        done,
                    )?);
                    position["part"] = part.clone();
                    events.push(self.emit("response.content_part.done", position)?);
                }
            }
            events.push(self.emit(
                "response.output_item.done",
                json!({
                    "output_index": output_index, "item": item,
                }),
            )?);
        }
        events.push(self.emit(&format!("response.{status}"), json!({"response": response}))?);
        self.response = Some(response);
        self.finished = true;
        Ok(events)
    }
}
