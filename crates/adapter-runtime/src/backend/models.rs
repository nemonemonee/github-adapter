use crate::selection::Capability;
use crate::transport;
use adapter_protocol::{AdapterError, Result, Value};
use serde_json::{Map, json};
use std::collections::{HashMap, HashSet};

pub(super) const SOL: &str = "gpt-5.6-sol";
#[cfg(test)]
pub(super) const SOL_FAST: &str = "gpt-5.6-sol-fast";
const CODING_INSTRUCTIONS: &str = "You are a coding assistant. Follow the user's instructions, inspect the project before changing it, and use the provided tools when appropriate.";

#[derive(Clone, Debug)]
pub(super) struct Model {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub responses: Capability<()>,
    pub chat: Capability<()>,
    pub tools: Capability<()>,
    pub parallel: Capability<()>,
    pub vision: Capability<()>,
    pub reasoning: Capability<Vec<String>>,
    pub context: Option<u64>,
    pub prompt_limit: Option<u64>,
    pub output_limit: Option<u64>,
    fast_eligible: bool,
}

impl Model {
    pub fn native(&self) -> bool {
        matches!(self.responses, Capability::Supported(()))
    }
    pub fn direct_chat(&self) -> bool {
        matches!(self.chat, Capability::Supported(()))
    }
    pub fn supports_tools(&self) -> bool {
        matches!(self.tools, Capability::Supported(()))
    }

    pub fn validate_effort(&self, effort: &Value) -> Result<()> {
        if effort.is_null() {
            return Ok(());
        }
        let effort = effort
            .as_str()
            .filter(|effort| !effort.trim().is_empty())
            .ok_or_else(|| AdapterError::invalid("reasoning effort must be a nonempty string."))?;
        match &self.reasoning {
            Capability::Unsupported => Err(AdapterError::invalid(
                "The selected model explicitly does not support reasoning effort.",
            )),
            Capability::Supported(efforts)
                if !efforts.is_empty() && !efforts.iter().any(|allowed| allowed == effort) =>
            {
                Err(AdapterError::invalid(
                    "The selected model does not advertise that reasoning effort.",
                ))
            }
            _ => Ok(()),
        }
    }
}

pub(super) struct Catalog {
    pub models: Vec<Model>,
    pub value: Value,
}

fn upstream_object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| AdapterError::upstream(format!("The provider returned invalid {label}.")))
}

fn flag(value: Option<&Value>, label: &str) -> Result<Capability<()>> {
    match value {
        None | Some(Value::Null) => Ok(Capability::Unknown),
        Some(Value::Bool(true)) => Ok(Capability::Supported(())),
        Some(Value::Bool(false)) => Ok(Capability::Unsupported),
        _ => Err(AdapterError::upstream(format!(
            "The provider returned an invalid {label} capability."
        ))),
    }
}

fn efforts(value: Option<&Value>) -> Result<Capability<Vec<String>>> {
    match value {
        None | Some(Value::Null) => Ok(Capability::Unknown),
        Some(Value::Bool(false)) => Ok(Capability::Unsupported),
        Some(Value::Bool(true)) => Ok(Capability::Supported(Vec::new())),
        Some(Value::Array(values)) => {
            let mut seen = HashSet::new();
            let mut result = Vec::new();
            for value in values {
                let effort = value
                    .as_str()
                    .filter(|effort| !effort.trim().is_empty())
                    .ok_or_else(|| {
                        AdapterError::upstream("The provider returned invalid reasoning efforts.")
                    })?;
                if seen.insert(effort) {
                    result.push(effort.to_owned());
                }
            }
            Ok(Capability::Supported(result))
        }
        _ => Err(AdapterError::upstream(
            "The provider returned invalid reasoning efforts.",
        )),
    }
}

fn limit(object: &Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => transport::unsigned(value)
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| {
                AdapterError::upstream(format!(
                    "The provider returned an invalid model limit: {key}."
                ))
            }),
    }
}

fn optional_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    empty: &'a Map<String, Value>,
) -> Result<&'a Map<String, Value>> {
    match object.get(key) {
        None => Ok(empty),
        Some(value) => upstream_object(value, key),
    }
}

fn parse_github(value: &Value) -> Result<Option<Model>> {
    let record = upstream_object(value, "model record")?;
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| AdapterError::upstream("Copilot returned an invalid model ID."))?;
    if record.get("model_picker_enabled") != Some(&Value::Bool(true)) {
        return Ok(None);
    }
    if let Some(policy) = record.get("policy").filter(|value| !value.is_null()) {
        let policy = upstream_object(policy, "model policy")?;
        if policy
            .get("state")
            .is_some_and(|state| !state.is_null() && state != "enabled")
        {
            return Ok(None);
        }
    }
    let empty = Map::new();
    let capabilities = optional_object(record, "capabilities", &empty)?;
    if capabilities
        .get("type")
        .is_some_and(|kind| !kind.is_null() && kind != "chat")
    {
        return Ok(None);
    }
    let endpoints = match record.get("supported_endpoints") {
        None => return Ok(None),
        Some(Value::Array(endpoints)) => endpoints,
        _ => {
            return Err(AdapterError::upstream(
                "Copilot returned invalid supported endpoints.",
            ));
        }
    };
    let mut responses = false;
    let mut chat = false;
    for endpoint in endpoints {
        match endpoint.as_str().ok_or_else(|| {
            AdapterError::upstream("Copilot returned invalid supported endpoints.")
        })? {
            "/responses" | "/v1/responses" => responses = true,
            "/chat/completions" | "/v1/chat/completions" => chat = true,
            _ => {}
        }
    }
    if !responses && !chat {
        return Ok(None);
    }
    let limits = optional_object(capabilities, "limits", &empty)?;
    let supports = optional_object(capabilities, "supports", &empty)?;
    let name = match record.get("name") {
        None => id,
        Some(value) => value
            .as_str()
            .ok_or_else(|| AdapterError::upstream("Copilot returned invalid model names."))?,
    };
    let vendor = record.get("vendor").or_else(|| record.get("owned_by"));
    let vendor = match vendor {
        None => "GitHub Copilot",
        Some(value) => value
            .as_str()
            .ok_or_else(|| AdapterError::upstream("Copilot returned invalid model names."))?,
    };
    let prompt_limit = limit(limits, "max_prompt_tokens")?;
    Ok(Some(Model {
        id: id.to_owned(),
        name: name.to_owned(),
        vendor: vendor.to_owned(),
        responses: if responses {
            Capability::Supported(())
        } else {
            Capability::Unsupported
        },
        chat: if chat {
            Capability::Supported(())
        } else {
            Capability::Unsupported
        },
        tools: flag(supports.get("tool_calls"), "tool_calls")?,
        parallel: flag(supports.get("parallel_tool_calls"), "parallel_tool_calls")?,
        vision: flag(supports.get("vision"), "vision")?,
        reasoning: efforts(supports.get("reasoning_effort"))?,
        context: limit(limits, "max_context_window_tokens")?.or(prompt_limit),
        prompt_limit,
        output_limit: limit(limits, "max_output_tokens")?,
        fast_eligible: responses
            && !chat
            && vendor.eq_ignore_ascii_case("openai")
            && record
                .get("policy")
                .and_then(|policy| policy.get("state"))
                .and_then(Value::as_str)
                == Some("enabled"),
    }))
}

fn codex(model: &Model, priority: usize, fast: bool) -> Value {
    let efforts = match &model.reasoning {
        Capability::Supported(efforts) => efforts.as_slice(),
        _ => &[],
    };
    let default_effort = efforts
        .iter()
        .find(|effort| effort.as_str() == "medium")
        .or_else(|| efforts.first());
    let percent = match (model.context, model.prompt_limit) {
        (Some(context), Some(prompt)) => {
            (u128::from(prompt) * 100 / u128::from(context)).clamp(1, 95) as u64
        }
        _ => 95,
    };
    let compact_limit = model
        .prompt_limit
        .or(model.context)
        .map(|limit| (u128::from(limit) * 9 / 10) as u64);
    let transport = if model.native() {
        "native Responses"
    } else {
        "streaming Chat Completions"
    };
    json!({
        "slug": model.id,
        "display_name": format!("{} (Personal GitHub)", model.name),
        "description": format!("{} via {transport}.", model.vendor),
        "default_reasoning_level": default_effort,
        "supported_reasoning_levels": efforts.iter().map(|effort| {
            let mut characters = effort.chars();
            let capitalized = characters.next().map(|first| first.to_uppercase().collect::<String>() + characters.as_str()).unwrap_or_default();
            json!({"effort": effort, "description": format!("{capitalized} reasoning")})
        }).collect::<Vec<_>>(),
        "shell_type": if model.supports_tools() { "unified_exec" } else { "disabled" },
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "additional_speed_tiers": if fast { json!(["fast"]) } else { json!([]) },
        "service_tiers": if fast { json!([
            {"id": "default", "name": "Standard", "description": "Use the model's default service tier."},
            {"id": "priority", "name": "Fast", "description": "Use the Fast model advertised by this Copilot account."},
        ]) } else { json!([]) },
        "default_service_tier": if fast { json!("default") } else { Value::Null },
        "availability_nux": null, "upgrade": null,
        "base_instructions": CODING_INSTRUCTIONS,
        "model_messages": {"instructions_template": CODING_INSTRUCTIONS, "instructions_variables": null},
        "supports_reasoning_summaries": false,
        "supports_reasoning_summary_parameter": false,
        "default_reasoning_summary": "none",
        "support_verbosity": false, "default_verbosity": null, "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "bytes", "limit": 10_000},
        "context_window": model.context, "max_context_window": model.context,
        "effective_context_window_percent": percent,
        "auto_compact_token_limit": compact_limit,
        "supports_parallel_tool_calls": matches!(model.parallel, Capability::Supported(())),
        "experimental_supported_tools": [],
        "input_modalities": if matches!(model.vision, Capability::Supported(())) { json!(["text", "image"]) } else { json!(["text"]) },
        "prefer_websockets": false,
    })
}

pub(super) fn github(payload: Value, account: &str) -> Result<Catalog> {
    let records = match &payload {
        Value::Array(records) => records,
        Value::Object(fields) => fields
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| AdapterError::upstream("Copilot did not return a model list."))?,
        _ => {
            return Err(AdapterError::upstream(
                "Copilot did not return a model list.",
            ));
        }
    };
    let mut models = Vec::new();
    let mut ids = HashSet::new();
    for record in records {
        if let Some(model) = parse_github(record)? {
            if !ids.insert(model.id.clone()) {
                return Err(AdapterError::upstream(
                    "Copilot returned duplicate enabled model IDs.",
                ));
            }
            models.push(model);
        }
    }
    if models.is_empty() {
        return Err(AdapterError::new(
            403,
            "no_compatible_models",
            "The selected GitHub account advertises no enabled compatible models. Check Copilot access and model policies; no other account was used.",
        ));
    }
    let value = json!({
        "models": models.iter().enumerate().map(|(index, model)| codex(model, index, fast_variant(&models, model).is_some())).collect::<Vec<_>>(),
        "object": "list",
        "data": models.iter().map(|model| json!({
            "id": model.id, "object": "model", "created": 0, "owned_by": model.vendor, "display_name": model.name,
        })).collect::<Vec<_>>(),
        "provider": "github", "account": account,
    });
    Ok(Catalog { models, value })
}

pub(super) fn mai(cache: Value, live: Value) -> Result<Catalog> {
    let cached = cache
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AdapterError::upstream("The MAI Codex model cache must contain a model list.")
        })?;
    let records = live.get("data").and_then(Value::as_array).ok_or_else(|| {
        AdapterError::upstream("MAI model discovery returned an invalid model list.")
    })?;
    let mut available = HashMap::new();
    let mut live_ids = Vec::new();
    for record in records {
        let object = upstream_object(record, "MAI model record")?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                AdapterError::upstream("MAI model discovery returned an invalid model record.")
            })?;
        if available.insert(id, record).is_some() {
            return Err(AdapterError::upstream(
                "MAI model discovery returned duplicate model IDs.",
            ));
        }
        live_ids.push(id);
    }
    let mut rich = Vec::new();
    let mut rich_by_id = HashMap::new();
    for cached in cached {
        let Some(slug) = cached.get("slug").and_then(Value::as_str) else {
            continue;
        };
        if !available.contains_key(slug) {
            continue;
        }
        let mut model = cached.clone();
        model["additional_speed_tiers"] = json!([]);
        model["service_tiers"] = json!([]);
        let mut levels = match model.get("supported_reasoning_levels") {
            None => Vec::new(),
            Some(Value::Array(levels)) => {
                let mut result = Vec::new();
                for level in levels {
                    let effort = level
                        .get("effort")
                        .and_then(Value::as_str)
                        .filter(|effort| !effort.trim().is_empty())
                        .ok_or_else(|| {
                            AdapterError::upstream(
                                "The MAI cache contains invalid reasoning levels.",
                            )
                        })?;
                    if effort != "ultra" {
                        result.push(level.clone());
                    }
                }
                result
            }
            _ => {
                return Err(AdapterError::upstream(
                    "The MAI cache contains invalid reasoning levels.",
                ));
            }
        };
        if slug == SOL && !levels.iter().any(|level| level["effort"] == "max") {
            levels.push(json!({"effort": "max", "description": "Maximum reasoning depth"}));
        }
        model["supported_reasoning_levels"] = json!(levels);
        if model["default_reasoning_level"] == "ultra" {
            model["default_reasoning_level"] = json!("xhigh");
        }
        if rich_by_id.insert(slug.to_owned(), model.clone()).is_some() {
            return Err(AdapterError::upstream(
                "The MAI cache contains duplicate model IDs.",
            ));
        }
        rich.push(model);
    }
    if rich.is_empty() {
        return Err(AdapterError::upstream(
            "No rich Codex models in the configured cache match the MAI catalog.",
        ));
    }
    let mut models = Vec::new();
    for id in live_ids {
        let record = available[id];
        let cached = rich_by_id.get(id).and_then(Value::as_object);
        let empty = Map::new();
        let cached = cached.unwrap_or(&empty);
        let name = record
            .get("display_name")
            .or_else(|| record.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(id);
        let vendor = record
            .get("owned_by")
            .and_then(Value::as_str)
            .unwrap_or("MAI");
        let reasoning = cached
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .map(|levels| {
                Capability::Supported(
                    levels
                        .iter()
                        .filter_map(|level| level["effort"].as_str().map(str::to_owned))
                        .collect(),
                )
            })
            .unwrap_or(Capability::Unknown);
        models.push(Model {
            id: id.to_owned(),
            name: name.to_owned(),
            vendor: vendor.to_owned(),
            responses: Capability::Unknown,
            chat: Capability::Unknown,
            tools: Capability::Unknown,
            parallel: flag(
                cached.get("supports_parallel_tool_calls"),
                "parallel_tool_calls",
            )?,
            vision: Capability::Unknown,
            reasoning,
            context: limit(cached, "context_window")?.or(limit(cached, "max_context_window")?),
            prompt_limit: limit(cached, "max_prompt_tokens")?,
            output_limit: limit(cached, "max_output_tokens")?,
            fast_eligible: false,
        });
    }
    let mut data = Vec::new();
    for model in &rich {
        let id = model["slug"]
            .as_str()
            .ok_or_else(|| AdapterError::upstream("The MAI cache contains an invalid model ID."))?;
        let mut record = available
            .get(id)
            .and_then(|record| record.as_object())
            .cloned()
            .ok_or_else(|| AdapterError::upstream("The MAI catalog lost its model metadata."))?;
        record.entry("object").or_insert_with(|| json!("model"));
        record.entry("created").or_insert_with(|| json!(0));
        record.entry("owned_by").or_insert_with(|| json!("MAI"));
        data.push(Value::Object(record));
    }
    Ok(Catalog {
        models,
        value: json!({"models": rich, "object": "list", "data": data, "provider": "mai", "account": null}),
    })
}

pub(super) fn select<'a>(
    catalog: &'a Catalog,
    model: &str,
    tier: &Value,
    github: bool,
) -> Result<&'a Model> {
    let selected = catalog.models.iter().find(|candidate| candidate.id == model)
        .ok_or_else(|| AdapterError::invalid(format!("Model '{model}' is not available from the selected provider. No alias or alternate account was substituted.")))?;
    if !github || tier.is_null() || tier == "auto" || tier == "default" {
        return Ok(selected);
    }
    if tier != "priority" {
        return Err(AdapterError::invalid(
            "Unsupported service_tier; use default or priority.",
        ));
    }
    if selected.id.ends_with("-fast") && selected.fast_eligible {
        return Ok(selected);
    }
    if let Some(fast) = fast_variant(&catalog.models, selected) {
        return Ok(fast);
    }
    Err(AdapterError::invalid(
        "Fast is not advertised for this model/account. No Standard fallback was used.",
    ))
}

fn fast_variant<'a>(models: &'a [Model], base: &Model) -> Option<&'a Model> {
    if !base.id.starts_with("gpt-") || base.id.ends_with("-fast") || !base.native() {
        return None;
    }
    let id = format!("{}-fast", base.id);
    models.iter().find(|candidate| {
        candidate.id == id && candidate.fast_eligible && candidate.supports_tools()
    })
}
