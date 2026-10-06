//! Pure request validation and provider controls, without I/O or history access.

use super::models::Model;
use crate::history::input_items;
use crate::selection::Provider;
use crate::transport;
use adapter_protocol::{AdapterError, Result, Value};
use serde_json::json;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RequestKind {
    Responses { stream: bool },
    Chat { stream: bool },
    Compaction,
}

impl RequestKind {
    pub(super) fn is_chat(self) -> bool {
        matches!(self, Self::Chat { .. })
    }

    pub(super) fn stream(self) -> bool {
        match self {
            Self::Responses { stream } | Self::Chat { stream } => stream,
            Self::Compaction => false,
        }
    }
}

pub(super) fn validate_shape(
    payload: &Value,
    kind: RequestKind,
    provider: Provider,
) -> Result<&str> {
    let object = payload
        .as_object()
        .ok_or_else(|| AdapterError::invalid("The request must be a JSON object."))?;
    let requested = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            AdapterError::invalid("A model ID from the selected provider is required.")
        })?;
    for key in ["stream", "store", "background"] {
        if object
            .get(key)
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            return Err(AdapterError::invalid(format!("{key} must be a boolean.")));
        }
    }
    if kind.is_chat() {
        if !object
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| !messages.is_empty() && messages.iter().all(Value::is_object))
        {
            return Err(AdapterError::invalid(
                "messages must be a nonempty list of message objects.",
            ));
        }
    } else if let Some(input) = object.get("input") {
        input_items(Some(input))?;
    }
    let github = provider == Provider::Github;
    let token_fields: &[&str] = if kind.is_chat() {
        &["max_tokens", "max_completion_tokens"]
    } else {
        &["max_output_tokens"]
    };
    for key in token_fields {
        if let Some(value) = object.get(*key).filter(|value| !value.is_null()) {
            transport::positive(value, key)?;
        }
    }
    if let Some(tools) = object.get("tools")
        && (!tools.is_null() || (github && !kind.is_chat()))
        && !tools
            .as_array()
            .is_some_and(|tools| tools.iter().all(Value::is_object))
    {
        return Err(AdapterError::invalid(
            "tools must be an array of tool objects.",
        ));
    }
    if object
        .get("parallel_tool_calls")
        .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(AdapterError::invalid(
            "parallel_tool_calls must be a boolean.",
        ));
    }
    if !kind.is_chat() {
        if object
            .get("reasoning")
            .is_some_and(|value| !value.is_null() && !value.is_object())
        {
            return Err(AdapterError::invalid("reasoning must be an object."));
        }
        if object.get("previous_response_id").is_some_and(|value| {
            !value.is_null() && !value.as_str().is_some_and(|id| !id.is_empty())
        }) {
            return Err(AdapterError::invalid(
                "previous_response_id must be a nonempty string.",
            ));
        }
    }
    if github {
        if !kind.is_chat()
            && object
                .get("background")
                .is_some_and(|value| !value.is_null() && value != false)
        {
            return Err(AdapterError::invalid(
                "Personal mode does not support background Responses.",
            ));
        }
        if !kind.is_chat()
            && object
                .get("conversation")
                .is_some_and(|value| !value.is_null())
        {
            return Err(AdapterError::invalid(
                "Use local previous_response_id instead of a remote conversation.",
            ));
        }
        if kind.is_chat()
            && object
                .get("store")
                .is_some_and(|value| !value.is_null() && value != false)
        {
            return Err(AdapterError::invalid(
                "Direct Chat Completions does not support remote storage.",
            ));
        }
    }
    if kind == RequestKind::Compaction
        && object
            .get("stream")
            .is_some_and(|value| !value.is_null() && value != false)
    {
        return Err(AdapterError::invalid(
            "Compaction returns JSON; stream must be false.",
        ));
    }
    Ok(requested)
}

pub(super) fn cap_tokens(request: &mut Value, key: &str, limit: Option<u64>) -> Result<()> {
    if let Some(value) = request.get(key).filter(|value| !value.is_null()) {
        let tokens = transport::positive(value, key)?;
        if let Some(limit) = limit {
            request[key] = json!(tokens.min(limit));
        }
    }
    Ok(())
}

pub(super) fn validate_github_controls(
    request: &Value,
    model: &Model,
    kind: RequestKind,
) -> Result<()> {
    if kind.is_chat() {
        if let Some(effort) = request.get("reasoning_effort") {
            model.validate_effort(effort)?;
        }
        return Ok(());
    }
    if let Some(reasoning) = request.get("reasoning").filter(|value| !value.is_null()) {
        if !reasoning.is_object() {
            return Err(AdapterError::invalid("reasoning must be an object."));
        }
        if let Some(effort) = reasoning.get("effort") {
            model.validate_effort(effort)?;
        }
    }
    if let Some(tools) = request.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| AdapterError::invalid("tools must be a list."))?;
        if !tools.iter().all(Value::is_object) {
            return Err(AdapterError::invalid("Each tool must be an object."));
        }
        if !tools.is_empty() && !model.supports_tools() {
            return Err(AdapterError::invalid(
                "This model does not advertise tool support.",
            ));
        }
        if tools.iter().any(|tool| tool["type"] == "image_generation") {
            return Err(AdapterError::invalid(
                "GitHub Copilot does not provide the hosted image_generation tool.",
            ));
        }
    }
    if request
        .get("parallel_tool_calls")
        .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(AdapterError::invalid(
            "parallel_tool_calls must be a boolean.",
        ));
    }
    Ok(())
}

pub(super) fn remove_unsupported_image_tools(request: &mut Value) -> Result<()> {
    let unsupported = |value: &Value| match value["type"].as_str() {
        Some("image_generation" | "image_gen") => true,
        Some("namespace") => value["name"] == "image_gen",
        Some("function") => {
            let function = value.get("function").unwrap_or(value);
            function["name"] == "image_gen.imagegen"
                || (function["namespace"] == "image_gen" && function["name"] == "imagegen")
        }
        _ => false,
    };
    let choice = &request["tool_choice"];
    if unsupported(choice)
        || (choice["type"] == "allowed_tools"
            && choice["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(unsupported)))
    {
        return Err(AdapterError::invalid(
            "The explicitly selected hosted image tool is unavailable. Use a separately configured image provider.",
        ));
    }
    let required = request["tool_choice"] == "required";
    if let Some(tools) = request.get_mut("tools").filter(|value| !value.is_null()) {
        let tools = tools
            .as_array_mut()
            .ok_or_else(|| AdapterError::invalid("tools must be an array."))?;
        if !tools.iter().all(Value::is_object) {
            return Err(AdapterError::invalid("Each tool must be an object."));
        }
        let removed = tools.iter().any(unsupported);
        tools.retain(|tool| !unsupported(tool));
        if removed && required && tools.is_empty() {
            return Err(AdapterError::invalid(
                "The request requires a hosted image tool that this provider cannot execute.",
            ));
        }
    }
    Ok(())
}

pub(super) fn initiator(payload: Option<&Value>) -> &'static str {
    let last = payload
        .and_then(|payload| payload.get("input").or_else(|| payload.get("messages")))
        .and_then(Value::as_array)
        .and_then(|history| history.last());
    if last.is_some_and(|last| {
        last["type"] == "function_call_output"
            || matches!(last["role"].as_str(), Some("tool" | "assistant"))
    }) {
        "agent"
    } else {
        "user"
    }
}

pub(super) fn contains_image(payload: &Value) -> bool {
    payload
        .get("input")
        .or_else(|| payload.get("messages"))
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            matches!(part["type"].as_str(), Some("input_image" | "image_url"))
                        })
                    })
            })
        })
}
