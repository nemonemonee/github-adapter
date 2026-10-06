use serde_json::{Map, Value, json};

use crate::error::{AdapterError, Result};
use crate::json::nonnegative_integer;

fn count(value: Option<&Value>) -> Result<u64> {
    value.and_then(nonnegative_integer).ok_or_else(|| {
        AdapterError::invalid("Token counts must be nonnegative integers representable as u64.")
    })
}

fn sum(left: u64, right: u64) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| AdapterError::invalid("Token counts exceed the supported integer range."))
}

pub fn convert_usage(value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let usage = value
        .as_object()
        .ok_or_else(|| AdapterError::invalid("completion.usage must be an object."))?;
    let inputs = count(usage.get("prompt_tokens"))?;
    let mut outputs = count(usage.get("completion_tokens"))?;
    let reasoning = match usage.get("reasoning_tokens") {
        None | Some(Value::Null) => None,
        value => Some(count(value)?),
    };
    if reasoning.is_some_and(|tokens| tokens != 0) && !usage.contains_key("total_tokens") {
        return Err(AdapterError::invalid(
            "Standalone reasoning token counts require total_tokens.",
        ));
    }
    let base_total = sum(inputs, outputs)?;
    let total = match usage.get("total_tokens") {
        None => base_total,
        value => count(value)?,
    };
    if total != base_total {
        let extra = reasoning.ok_or_else(|| {
            AdapterError::invalid(
                "completion.usage.total_tokens is not accounted for by input, output, and reasoning.",
            )
        })?;
        if total != sum(base_total, extra)? {
            return Err(AdapterError::invalid(
                "completion.usage.total_tokens is not accounted for by input, output, and reasoning.",
            ));
        }
        outputs = sum(outputs, extra)?;
    }
    let mut result = Map::from_iter([
        ("input_tokens".into(), json!(inputs)),
        ("output_tokens".into(), json!(outputs)),
        ("total_tokens".into(), json!(total)),
    ]);
    for (source, target, key, limit) in [
        (
            "prompt_tokens_details",
            "input_tokens_details",
            "cached_tokens",
            inputs,
        ),
        (
            "completion_tokens_details",
            "output_tokens_details",
            "reasoning_tokens",
            outputs,
        ),
    ] {
        let details = match usage.get(source) {
            None | Some(Value::Null) => continue,
            Some(value) => value.as_object().ok_or_else(|| {
                AdapterError::invalid("completion.usage token details must be objects.")
            })?,
        };
        for value in details.values().filter(|value| !value.is_null()) {
            count(Some(value))?;
        }
        if let Some(value) = details.get(key).filter(|value| !value.is_null()) {
            let tokens = count(Some(value))?;
            if tokens > limit {
                return Err(AdapterError::invalid(
                    "completion.usage token details exceed their token count.",
                ));
            }
            result.insert(
                target.into(),
                Value::Object(Map::from_iter([(key.into(), json!(tokens))])),
            );
        }
    }
    if let Some(reasoning) = reasoning {
        if reasoning > outputs {
            return Err(AdapterError::invalid(
                "completion.usage.reasoning_tokens exceeds its output token count.",
            ));
        }
        if result
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .is_some_and(|existing| existing != reasoning)
        {
            return Err(AdapterError::invalid(
                "completion.usage contains conflicting reasoning token counts.",
            ));
        }
        result.insert(
            "output_tokens_details".into(),
            json!({"reasoning_tokens": reasoning}),
        );
    }
    Ok(Value::Object(result))
}
