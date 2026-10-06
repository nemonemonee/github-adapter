use serde_json::{Value, json};

use crate::error::{AdapterError, Result};

pub fn request(value: &Value) -> Result<Value> {
    let source = value
        .as_object()
        .ok_or_else(|| AdapterError::invalid("A compaction request must be an object."))?;
    match source.get("stream") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        _ => {
            return Err(AdapterError::invalid(
                "Compaction returns JSON; stream must be false.",
            ));
        }
    }
    let mut items = match source.get("input") {
        Some(Value::String(text)) => vec![json!({"role": "user", "content": text})],
        Some(Value::Array(items)) if items.iter().all(Value::is_object) => items.clone(),
        None => Vec::new(),
        _ => {
            return Err(AdapterError::invalid(
                "input must be a string or a list of input objects.",
            ));
        }
    };
    items.retain(|item| item.get("type").and_then(Value::as_str) != Some("compaction_trigger"));
    if items.is_empty() {
        return Err(AdapterError::invalid(
            "Compaction requires conversation input.",
        ));
    }
    items.push(json!({"type": "compaction_trigger"}));
    let mut result = source.clone();
    result.insert("input".into(), Value::Array(items));
    result.insert("stream".into(), Value::Bool(false));
    Ok(Value::Object(result))
}

pub fn response(value: &Value) -> Result<Value> {
    let source = value.as_object().ok_or_else(|| {
        AdapterError::upstream("Copilot returned an invalid compaction response.")
    })?;
    let output = source.get("output").and_then(Value::as_array);
    if source
        .get("id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || output.is_none()
        || source
            .get("status")
            .is_some_and(|status| status.as_str() != Some("completed"))
    {
        return Err(AdapterError::upstream(
            "Copilot returned an invalid compaction response.",
        ));
    }
    let mut found = false;
    for item in output.into_iter().flatten() {
        if item.get("type").and_then(Value::as_str) != Some("compaction") {
            continue;
        }
        found = true;
        if item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_none_or(|text| text.trim().is_empty())
        {
            return Err(missing_ciphertext());
        }
    }
    if !found {
        return Err(missing_ciphertext());
    }
    let mut result = source.clone();
    result.insert("object".into(), json!("response.compaction"));
    Ok(Value::Object(result))
}

fn missing_ciphertext() -> AdapterError {
    AdapterError::upstream(
        "Copilot did not return genuine encrypted compaction output. No history was discarded.",
    )
}
