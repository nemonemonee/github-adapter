//! Buffered Anthropic codecs. Personal-provider request policy is deliberately
//! separate from legacy conversion; the caller merges validated request extras.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::chat::{
    Object, array, as_upstream, boolean, field, fields, finite_json, object, one_of, python_json,
    string, uint,
};
use crate::error::{AdapterError, Result};
use crate::sse::SseEvent;

const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

fn nonempty<'a>(value: &'a Value, path: &str) -> Result<&'a str> {
    let value = string(value, path)?;
    if value.is_empty() {
        return Err(AdapterError::invalid(format!(
            "{path} must be a nonempty string"
        )));
    }
    Ok(value)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Number(value) => value.as_f64() != Some(0.0),
    }
}

fn text_blocks(value: &Value, path: &str) -> Result<()> {
    if value.is_string() {
        return Ok(());
    }
    for block in array(value, path)? {
        let block = object(block, path)?;
        if field(block, "type") != "text" || !field(block, "text").is_string() {
            return Err(AdapterError::invalid(format!(
                "{path} must contain text, not unsupported content blocks."
            )));
        }
    }
    Ok(())
}

fn content_block(value: &Value, role: &str) -> Result<()> {
    let block = object(value, "Anthropic content block")?;
    match field(block, "type").as_str() {
        Some("text") => {
            string(field(block, "text"), "text block text")?;
        }
        Some(kind @ ("image" | "document")) if role == "user" => {
            let source = object(field(block, "source"), "Image/document source")?;
            if kind == "image" && field(source, "type") == "url" {
                nonempty(field(source, "url"), "image URL")?;
            } else if field(source, "type") == "base64" {
                nonempty(field(source, "data"), "base64 data")?;
                nonempty(field(source, "media_type"), "base64 media_type")?;
            } else {
                return Err(AdapterError::invalid(
                    "Unsupported image/document source type.",
                ));
            }
        }
        Some("tool_use") if role == "assistant" => {
            nonempty(field(block, "id"), "tool_use id")?;
            nonempty(field(block, "name"), "tool_use name")?;
            object(field(block, "input"), "tool_use input")?;
        }
        Some("tool_result") if role == "user" => {
            nonempty(field(block, "tool_use_id"), "tool_result tool_use_id")?;
            if let Some(content) = block.get("content") {
                text_blocks(content, "tool_result content")?;
            }
            if let Some(is_error) = block.get("is_error") {
                boolean(is_error, "tool_result is_error")?;
            }
        }
        _ => {
            return Err(AdapterError::invalid(
                "This Anthropic content block cannot be represented by this Responses bridge. \
                 Start a new conversation when changing providers; thinking/history \
                 from another provider is not translated.",
            ));
        }
    }
    Ok(())
}

/// Validate personal-provider representability and return extra Responses
/// controls (`temperature`, `top_p`, `parallel_tool_calls`), without mutation.
pub fn validate_request(payload: &Value) -> Result<Value> {
    finite_json(payload, "Anthropic request")?;
    let payload = object(payload, "Anthropic request")?;
    fields(
        payload,
        &[
            "model",
            "messages",
            "system",
            "max_tokens",
            "stream",
            "tools",
            "tool_choice",
            "output_config",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "thinking",
            "metadata",
            "service_tier",
            "context_management",
        ],
        "Anthropic request",
    )?;
    let messages = array(field(payload, "messages"), "messages")?;
    if messages.is_empty() {
        return Err(AdapterError::invalid("messages must be a nonempty list."));
    }
    for message in messages {
        let message = object(message, "Anthropic message")?;
        let role = one_of(
            field(message, "role"),
            &["user", "assistant"],
            "Anthropic message role",
        )?;
        let content = field(message, "content");
        if !content.is_string() {
            for block in array(content, "Message content")? {
                content_block(block, role)?;
            }
        }
    }
    if let Some(system) = payload.get("system") {
        text_blocks(system, "system")?;
    }
    let context = field(payload, "context_management");
    if !context.is_null() {
        let preserve = json!({"type": "clear_thinking_20251015", "keep": "all"});
        let valid = context.as_object().is_some_and(|context| {
            context.keys().all(|key| key == "edits")
                && context.get("edits").is_none_or(|edits| {
                    edits
                        .as_array()
                        .is_some_and(|edits| edits.iter().all(|edit| edit == &preserve))
                })
        });
        if !valid {
            return Err(AdapterError::invalid(
                "Only empty or preserve-all Anthropic context_management is supported; \
                 history is never discarded.",
            ));
        }
    }
    if let Some(max_tokens) = payload.get("max_tokens") {
        uint(max_tokens, "max_tokens", 1)?;
    }
    if let Some(stream) = payload.get("stream") {
        boolean(stream, "stream")?;
    }
    if let Some(tools) = payload.get("tools") {
        for tool in array(tools, "tools")? {
            let tool = object(tool, "tool")?;
            if !field(tool, "type").is_null() && field(tool, "type") != "custom" {
                return Err(AdapterError::invalid(
                    "Only named tools with input_schema objects are supported.",
                ));
            }
            nonempty(field(tool, "name"), "tool name")?;
            object(field(tool, "input_schema"), "tool input_schema")?;
        }
    }
    let mut extras = json!({});
    if !field(payload, "tool_choice").is_null() {
        let choice = object(field(payload, "tool_choice"), "tool_choice")?;
        let kind = one_of(
            field(choice, "type"),
            &["auto", "any", "none", "tool"],
            "tool_choice.type",
        )?;
        if kind == "tool" {
            nonempty(field(choice, "name"), "tool_choice.name")?;
        }
        if let Some(disable) = choice.get("disable_parallel_tool_use")
            && boolean(disable, "disable_parallel_tool_use")?
        {
            extras["parallel_tool_calls"] = json!(false);
        }
    }
    let empty = json!({});
    let config = object(
        payload.get("output_config").unwrap_or(&empty),
        "output_config",
    )?;
    fields(config, &["effort", "format"], "output_config")?;
    if let Some(effort) = config.get("effort") {
        one_of(effort, EFFORTS, "output_config.effort")?;
    }
    if !field(config, "format").is_null() {
        let format = object(field(config, "format"), "output_config.format")?;
        if field(format, "type") != "json_schema" || !field(format, "schema").is_object() {
            return Err(AdapterError::invalid(
                "Only json_schema output formats are supported.",
            ));
        }
    }
    if !field(payload, "thinking").is_null() {
        let thinking = object(field(payload, "thinking"), "thinking")?;
        if field(thinking, "type") != "disabled"
            && !(field(thinking, "type") == "adaptive" && config.contains_key("effort"))
        {
            return Err(AdapterError::invalid(
                "Anthropic thinking budgets are not supported. Use output_config.effort \
                 with a model that supports reasoning.",
            ));
        }
    }
    if !field(payload, "top_k").is_null() || truthy(field(payload, "stop_sequences")) {
        return Err(AdapterError::invalid(
            "top_k and stop_sequences cannot be mapped to Responses.",
        ));
    }
    if !field(payload, "service_tier").is_null() && field(payload, "service_tier") != "auto" {
        return Err(AdapterError::invalid("Unsupported Anthropic service_tier."));
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = payload.get(key) {
            if !value.as_f64().is_some_and(f64::is_finite) {
                return Err(AdapterError::invalid(format!(
                    "{key} must be a finite number."
                )));
            }
            extras[key] = value.clone();
        }
    }
    Ok(extras)
}

fn anthropic_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").is_some_and(|kind| kind == "text") {
                    let text = match block.get("text") {
                        Some(text) => string(text, "text block text")?,
                        None => "",
                    };
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
            }
            Ok(parts.join("\n"))
        }
        _ => Ok(String::new()),
    }
}

fn flush_pending(items: &mut Vec<Value>, pending: &mut Vec<Value>, role: &Value) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let content = if pending.iter().all(|part| part["type"] == "input_text") {
        let text: Result<Vec<&str>> = pending
            .iter()
            .map(|part| string(&part["text"], "text block text"))
            .collect();
        let text = text?.join("\n");
        pending.clear();
        json!(text)
    } else {
        Value::Array(std::mem::take(pending))
    };
    items.push(json!({"role": role, "content": content}));
    Ok(())
}

fn legacy_source_string<'a>(source: &'a Object, key: &str, default: &'a str) -> Result<&'a str> {
    match source.get(key) {
        Some(value) => string(value, &format!("source.{key}")),
        None => Ok(default),
    }
}

fn responses_input(payload: &Object) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let Some(messages) = payload.get("messages") else {
        return Ok(items);
    };
    for message in array(messages, "messages")? {
        let Some(message) = message.as_object() else {
            continue;
        };
        let role = field(message, "role");
        let content = field(message, "content");
        if content.is_string() {
            items.push(json!({"role": role, "content": content}));
            continue;
        }
        let Some(content) = content.as_array() else {
            continue;
        };
        let mut pending = Vec::new();
        for raw in content {
            let Some(block) = raw.as_object() else {
                continue;
            };
            match field(block, "type").as_str() {
                Some("text") => {
                    let text = match block.get("text") {
                        Some(value) => string(value, "text block text")?,
                        None => "",
                    };
                    pending.push(json!({"type": "input_text", "text": text}));
                }
                Some("image") if role == "user" => {
                    let empty = json!({});
                    let source = object(block.get("source").unwrap_or(&empty), "image source")?;
                    match field(source, "type").as_str() {
                        Some("base64") => pending.push(json!({
                            "type": "input_image",
                            "image_url": format!(
                                "data:{};base64,{}",
                                legacy_source_string(source, "media_type", "image/png")?,
                                legacy_source_string(source, "data", "")?,
                            ),
                        })),
                        Some("url") => pending.push(json!({
                            "type": "input_image",
                            "image_url": legacy_source_string(source, "url", "")?,
                        })),
                        _ => {}
                    }
                }
                Some("document") if role == "user" => {
                    let empty = json!({});
                    let source = object(block.get("source").unwrap_or(&empty), "document source")?;
                    if field(source, "type") == "base64" {
                        pending.push(json!({
                            "type": "input_file",
                            "file_data": format!(
                                "data:{};base64,{}",
                                legacy_source_string(source, "media_type", "application/pdf")?,
                                legacy_source_string(source, "data", "")?,
                            ),
                            "filename": block.get("title").cloned().unwrap_or_else(|| json!("document.pdf")),
                        }));
                    }
                }
                Some("tool_use") if role == "assistant" => {
                    flush_pending(&mut items, &mut pending, role)?;
                    let empty = json!({});
                    let input = block.get("input").unwrap_or(&empty);
                    object(input, "tool_use input")?;
                    items.push(json!({
                        "type": "function_call",
                        "call_id": nonempty(field(block, "id"), "tool_use id")?,
                        "name": nonempty(field(block, "name"), "tool_use name")?,
                        "arguments": python_json(input, false)?,
                    }));
                }
                Some("tool_result") if role == "user" => {
                    flush_pending(&mut items, &mut pending, role)?;
                    let mut output = anthropic_text(field(block, "content"))?;
                    if truthy(field(block, "is_error")) {
                        output = format!("Error: {output}");
                    }
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": nonempty(field(block, "tool_use_id"), "tool_result tool_use_id")?,
                        "output": output,
                    }));
                }
                _ => {}
            }
        }
        flush_pending(&mut items, &mut pending, role)?;
    }
    Ok(items)
}

fn responses_tools(payload: &Object) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let Some(tools) = payload.get("tools") else {
        return Ok(result);
    };
    for tool in array(tools, "tools")? {
        let Some(tool) = tool.as_object() else {
            continue;
        };
        if !truthy(field(tool, "name")) {
            continue;
        }
        result.push(json!({
            "type": "function",
            "name": string(field(tool, "name"), "tool name")?,
            "description": tool.get("description").cloned().unwrap_or_else(|| json!("")),
            "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type": "object"})),
            "strict": false,
        }));
    }
    Ok(result)
}

fn responses_tool_choice(choice: &Value) -> Result<Option<Value>> {
    let Some(choice) = choice.as_object() else {
        return Ok(None);
    };
    Ok(match field(choice, "type").as_str() {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") if truthy(field(choice, "name")) => Some(json!({
            "type": "function", "name": string(field(choice, "name"), "tool_choice.name")?,
        })),
        _ => None,
    })
}

/// Pure legacy-compatible conversion. This does not call `validate_request` or
/// apply personal-only controls. Personal callers must validate and merge extras.
pub fn to_responses(
    payload: &Value,
    model: &str,
    force_effort: Option<&str>,
    max_tokens_cap: Option<u64>,
) -> Result<Value> {
    finite_json(payload, "Anthropic request")?;
    let payload = object(payload, "Anthropic request")?;
    if model.trim().is_empty() {
        return Err(AdapterError::invalid("model must be a nonempty string"));
    }
    if max_tokens_cap == Some(0) {
        return Err(AdapterError::invalid("max_tokens_cap must be positive"));
    }
    if force_effort.is_some_and(|effort| !EFFORTS.contains(&effort)) {
        return Err(AdapterError::invalid(
            "Unsupported forced reasoning effort.",
        ));
    }
    let mut request = json!({
        "model": model, "input": responses_input(payload)?, "stream": false,
    });
    let instructions = anthropic_text(field(payload, "system"))?;
    if !instructions.is_empty() {
        request["instructions"] = json!(instructions);
    }
    let tools = responses_tools(payload)?;
    if !tools.is_empty() {
        request["tools"] = json!(tools);
    }
    if let Some(choice) = responses_tool_choice(field(payload, "tool_choice"))? {
        request["tool_choice"] = choice;
    }
    if truthy(field(payload, "max_tokens")) {
        let tokens = uint(field(payload, "max_tokens"), "max_tokens", 1)?;
        request["max_output_tokens"] = json!(max_tokens_cap.map_or(tokens, |cap| tokens.min(cap)));
    }
    let config = field(payload, "output_config");
    let effort = force_effort.or_else(|| config.get("effort").and_then(Value::as_str));
    if let Some(effort) = effort.filter(|effort| EFFORTS.contains(effort)) {
        request["reasoning"] = json!({"effort": effort});
    }
    if let Some(format) = config.get("format").and_then(Value::as_object)
        && field(format, "type") == "json_schema"
    {
        request["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": format.get("name").cloned().unwrap_or_else(|| json!("response")),
                "schema": format.get("schema").cloned().unwrap_or_else(|| json!({})),
                "strict": true,
            },
        });
    }
    Ok(request)
}

fn checked_usage(response: &Value) -> Result<Value> {
    let usage = response
        .get("usage")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            AdapterError::upstream("Copilot omitted the usage required for an Anthropic response.")
        })?;
    let invalid_tokens = || AdapterError::upstream("Copilot returned invalid token usage.");
    let inputs =
        uint(field(usage, "input_tokens"), "input_tokens", 0).map_err(|_| invalid_tokens())?;
    let outputs =
        uint(field(usage, "output_tokens"), "output_tokens", 0).map_err(|_| invalid_tokens())?;
    if let Some(total) = usage.get("total_tokens") {
        let total = uint(total, "total_tokens", 0).map_err(|_| invalid_tokens())?;
        if inputs.checked_add(outputs) != Some(total) {
            return Err(AdapterError::upstream(
                "Copilot returned inconsistent token usage.",
            ));
        }
    }
    let details = field(usage, "input_tokens_details");
    let empty = json!({});
    let details = if details.is_null() { &empty } else { details };
    let details_object = details
        .as_object()
        .ok_or_else(|| AdapterError::upstream("Copilot returned invalid cache usage."))?;
    let mut counts = [0; 2];
    for (index, key) in ["cached_tokens", "cache_write_tokens"].iter().enumerate() {
        if let Some(value) = details_object.get(*key) {
            counts[index] = uint(value, key, 0)
                .map_err(|_| AdapterError::upstream("Copilot returned invalid cache usage."))?;
        }
    }
    if counts[0]
        .checked_add(counts[1])
        .is_none_or(|cached| cached > inputs)
    {
        return Err(AdapterError::upstream(
            "Copilot cache usage exceeds total input tokens.",
        ));
    }
    let mut usage = Value::Object(usage.clone());
    usage["input_tokens_details"] = details.clone();
    Ok(usage)
}

fn tool_input(item: &Value) -> Result<Value> {
    let raw = item
        .get("arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AdapterError::upstream("Copilot returned incomplete or invalid tool arguments.")
        })?;
    let input = crate::json::decode(raw.as_bytes(), raw.len()).map_err(|_| {
        AdapterError::upstream("Copilot returned incomplete or invalid tool arguments.")
    })?;
    finite_json(&input, "tool arguments").map_err(|_| {
        AdapterError::upstream("Copilot returned incomplete or invalid tool arguments.")
    })?;
    if !input.is_object()
        || !item
            .get("call_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        || !item
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty())
    {
        return Err(AdapterError::upstream(
            "Copilot returned an invalid function call.",
        ));
    }
    Ok(input)
}

struct ResponseComponents {
    usage: Value,
    content: Vec<Value>,
    has_tool: bool,
    has_refusal: bool,
}

fn response_components(response: &Value) -> Result<ResponseComponents> {
    finite_json(response, "response").map_err(as_upstream)?;
    object(response, "response").map_err(as_upstream)?;
    if response.get("error").is_some_and(|error| !error.is_null()) {
        return Err(AdapterError::upstream(
            "Copilot returned a failed response.",
        ));
    }
    if response
        .get("status")
        .is_some_and(|status| status != "completed" && status != "incomplete")
    {
        return Err(AdapterError::upstream(
            "Copilot did not return a final response.",
        ));
    }
    if response["status"] == "completed"
        && response
            .get("incomplete_details")
            .is_some_and(|details| !details.is_null())
    {
        return Err(AdapterError::upstream(
            "A completed response cannot have incomplete_details.",
        ));
    }
    let usage = checked_usage(response)?;
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| AdapterError::upstream("Copilot returned invalid output."))?;
    let mut content = Vec::new();
    let mut call_ids = BTreeSet::new();
    let mut has_tool = false;
    let mut has_refusal = false;
    for item in output {
        let item_object = item
            .as_object()
            .ok_or_else(|| AdapterError::upstream("Copilot returned an invalid output item."))?;
        match field(item_object, "type").as_str() {
            Some("function_call") => {
                let input = tool_input(item)?;
                let call_id = string(&item["call_id"], "function call ID").map_err(as_upstream)?;
                if !call_ids.insert(call_id.to_owned()) {
                    return Err(AdapterError::upstream(
                        "Copilot returned duplicate function call IDs.",
                    ));
                }
                has_tool = true;
                content.push(json!({
                    "type": "tool_use", "id": item["call_id"],
                    "name": item["name"], "input": input,
                }));
            }
            Some("message") => {
                let parts = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        AdapterError::upstream("Copilot returned invalid message content.")
                    })?;
                for part in parts {
                    let text = match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => part.get("text"),
                        Some("refusal") => {
                            has_refusal = true;
                            part.get("refusal")
                        }
                        _ => None,
                    }
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::upstream(
                            "Copilot output cannot be represented as an Anthropic message.",
                        )
                    })?;
                    content.push(json!({"type": "text", "text": text}));
                }
            }
            Some("reasoning") => {}
            _ => {
                return Err(AdapterError::upstream(
                    "Unsupported Copilot output for an Anthropic message.",
                ));
            }
        }
    }
    Ok(ResponseComponents {
        usage,
        content,
        has_tool,
        has_refusal,
    })
}

/// Check required usage and representable content. Invalid/partial tool JSON
/// never becomes a fabricated `raw` dictionary or an empty input object.
pub fn validate_response(response: &Value) -> Result<Value> {
    let components = response_components(response)?;
    let mut validated = response.clone();
    validated["usage"] = components.usage;
    Ok(validated)
}

fn stop_reason(response: &Value, has_tool: bool, has_refusal: bool) -> Result<&'static str> {
    if response["status"] == "incomplete" {
        let details = response.get("incomplete_details");
        return match details {
            None | Some(Value::Null) => Ok("max_tokens"),
            Some(details) => match details.get("reason").and_then(Value::as_str) {
                Some("max_output_tokens") => Ok("max_tokens"),
                Some("content_filter") => Ok("refusal"),
                _ => Err(AdapterError::upstream(
                    "Unsupported incomplete reason for an Anthropic message.",
                )),
            },
        };
    }
    Ok(if has_refusal {
        "refusal"
    } else if has_tool {
        "tool_use"
    } else {
        "end_turn"
    })
}

/// Convert a final Responses resource. Required usage is never defaulted to
/// zero; a content-filter stop maps to refusal, not to a token-limit stop.
pub fn to_message(response: &Value, requested_model: &str) -> Result<Value> {
    let components = response_components(response)?;
    if requested_model.trim().is_empty() {
        return Err(AdapterError::invalid(
            "requested_model must be a nonempty string",
        ));
    }
    let id = match response.get("id") {
        Some(id) => nonempty(id, "response.id").map_err(as_upstream)?,
        None => "msg_sol",
    };
    let usage = &components.usage;
    let details = &usage["input_tokens_details"];
    let creation = details["cache_write_tokens"].as_u64().unwrap_or(0);
    let read = details["cached_tokens"].as_u64().unwrap_or(0);
    let inputs = uint(&usage["input_tokens"], "input_tokens", 0).map_err(as_upstream)?;
    let inputs = inputs
        .checked_sub(creation)
        .and_then(|inputs| inputs.checked_sub(read))
        .ok_or_else(|| AdapterError::upstream("Copilot cache usage exceeds total input tokens."))?;
    let stop = stop_reason(response, components.has_tool, components.has_refusal)?;
    Ok(json!({
        "id": id.replace("resp_", "msg_"),
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": components.content,
        "stop_reason": stop,
        "stop_sequence": null,
        "usage": {
            "input_tokens": inputs,
            "output_tokens": usage["output_tokens"],
            "cache_creation_input_tokens": creation,
            "cache_read_input_tokens": read,
        },
    }))
}

/// Replay a buffered Anthropic message. These are snapshots, not a claim that
/// an upstream native Responses stream has been translated incrementally.
pub fn events(message: &Value) -> Result<Vec<SseEvent>> {
    (|| {
        finite_json(message, "Anthropic message")?;
        let data = object(message, "Anthropic message")?;
        if field(data, "type") != "message" || field(data, "role") != "assistant" {
            return Err(AdapterError::invalid("Expected an assistant Anthropic message."));
        }
        nonempty(field(data, "id"), "message.id")?;
        nonempty(field(data, "model"), "message.model")?;
        one_of(
            field(data, "stop_reason"),
            &["end_turn", "tool_use", "max_tokens", "refusal", "stop_sequence", "pause_turn"],
            "message.stop_reason",
        )?;
        if !field(data, "stop_sequence").is_null() {
            string(field(data, "stop_sequence"), "message.stop_sequence")?;
        }
        let usage = object(field(data, "usage"), "message.usage")?;
        uint(field(usage, "input_tokens"), "message.usage.input_tokens", 0)?;
        uint(field(usage, "output_tokens"), "message.usage.output_tokens", 0)?;
        for key in ["cache_creation_input_tokens", "cache_read_input_tokens"] {
            if let Some(count) = usage.get(key) {
                uint(count, &format!("message.usage.{key}"), 0)?;
            }
        }
        let content = array(field(data, "content"), "message.content")?;
        let mut encoded_inputs = Vec::with_capacity(content.len());
        let mut ids = BTreeSet::new();
        for block in content {
            let block = object(block, "message.content block")?;
            match field(block, "type").as_str() {
                Some("text") => {
                    string(field(block, "text"), "content.text")?;
                    encoded_inputs.push(None);
                }
                Some("tool_use") => {
                    let id = nonempty(field(block, "id"), "tool_use.id")?;
                    if !ids.insert(id) {
                        return Err(AdapterError::invalid("Duplicate Anthropic tool_use IDs."));
                    }
                    nonempty(field(block, "name"), "tool_use.name")?;
                    object(field(block, "input"), "tool_use.input")?;
                    encoded_inputs.push(Some(python_json(field(block, "input"), false)?));
                }
                _ => return Err(AdapterError::invalid("Unsupported Anthropic response content block.")),
            }
        }
        let mut result = Vec::new();
        let mut initial = message.clone();
        initial["content"] = json!([]);
        initial["stop_reason"] = Value::Null;
        initial["stop_sequence"] = Value::Null;
        result.push(SseEvent::json("message_start", json!({
            "type": "message_start", "message": initial,
        })));
        for (index, block) in content.iter().enumerate() {
            let initial = if block["type"] == "text" {
                json!({"type": "text", "text": ""})
            } else {
                json!({"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}})
            };
            result.push(SseEvent::json("content_block_start", json!({
                "type": "content_block_start", "index": index, "content_block": initial,
            })));
            if let Some(input) = &encoded_inputs[index] {
                result.push(SseEvent::json("content_block_delta", json!({
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": input},
                })));
            } else if !string(&block["text"], "content.text")?.is_empty() {
                result.push(SseEvent::json("content_block_delta", json!({
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "text_delta", "text": block["text"]},
                })));
            }
            result.push(SseEvent::json("content_block_stop", json!({
                "type": "content_block_stop", "index": index,
            })));
        }
        result.push(SseEvent::json("message_delta", json!({
            "type": "message_delta",
            "delta": {"stop_reason": message["stop_reason"], "stop_sequence": message["stop_sequence"]},
            "usage": {"output_tokens": usage["output_tokens"]},
        })));
        result.push(SseEvent::json("message_stop", json!({"type": "message_stop"})));
        Ok(result)
    })()
    .map_err(as_upstream)
}
