use super::*;
use serde::de::{MapAccess, Visitor};
use serde_json::{Map, Value, value::RawValue};
use toml_edit::{DocumentMut, Item};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub version: u32,
    pub entries: BTreeMap<Client, ManifestEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManifestEntry {
    pub target: String,
    pub created: bool,
    pub original_backup: Option<String>,
    pub original_sha256: Option<String>,
    pub post_sha256: String,
}

pub(super) fn serialize(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut raw = serde_json::to_vec_pretty(value)
        .map_err(|_| invalid("Configuration metadata could not be serialized."))?;
    raw.push(b'\n');
    if raw.len() > MAX_BYTES {
        return Err(invalid(
            "A generated settings or recovery file exceeds the 8 MiB safety limit.",
        ));
    }
    Ok(raw)
}

pub(super) fn json(raw: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| invalid("Settings or recovery metadata contain invalid UTF-8 JSON."))?;
    let value = json_value(text.trim_start_matches('\u{feff}'), 0)?;
    if !value.is_object() {
        return Err(invalid(
            "JSON settings and recovery metadata must be objects.",
        ));
    }
    Ok(value)
}

fn json_value(raw: &str, depth: usize) -> Result<Value> {
    if depth > 128 {
        return Err(invalid("JSON settings exceed the nesting safety limit."));
    }
    let raw = raw.trim();
    if raw.starts_with('{') {
        struct ObjectVisitor {
            depth: usize,
        }
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = Value;

            fn expecting(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
                output.write_str("an object without duplicate keys")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut access: A,
            ) -> std::result::Result<Value, A::Error> {
                let mut values = Map::new();
                while let Some((key, raw)) = access.next_entry::<String, Box<RawValue>>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                    let value = json_value(raw.get(), self.depth + 1)
                        .map_err(|_| serde::de::Error::custom("invalid nested JSON"))?;
                    values.insert(key, value);
                }
                Ok(Value::Object(values))
            }
        }
        let mut decoder = serde_json::Deserializer::from_str(raw);
        let value = serde::de::Deserializer::deserialize_map(&mut decoder, ObjectVisitor { depth })
            .map_err(|_| {
                invalid("JSON settings are malformed, nonfinite, or contain duplicate keys.")
            })?;
        decoder
            .end()
            .map_err(|_| invalid("JSON settings contain trailing content."))?;
        return Ok(value);
    }
    if raw.starts_with('[') {
        let values: Vec<Box<RawValue>> = serde_json::from_str(raw)
            .map_err(|_| invalid("JSON settings contain an invalid array."))?;
        return values
            .into_iter()
            .map(|value| json_value(value.get(), depth + 1))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array);
    }
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| invalid("JSON settings contain an invalid value."))?;
    if let Value::Number(number) = &value {
        let spelling = number.to_string();
        if spelling.contains(['.', 'e', 'E']) && !number.as_f64().is_some_and(f64::is_finite) {
            return Err(invalid("JSON settings must contain only finite numbers."));
        }
    }
    Ok(value)
}

pub(super) fn manifest(raw: Option<&[u8]>) -> Result<Manifest> {
    let Some(raw) = raw else {
        return Ok(Manifest {
            version: 1,
            entries: BTreeMap::new(),
        });
    };
    let value = json(raw)?;
    exact_keys(&value, &["version", "entries"])?;
    let entries = value["entries"]
        .as_object()
        .ok_or_else(|| invalid("The backup manifest entries must be an object."))?;
    for entry in entries.values() {
        exact_keys(
            entry,
            &[
                "target",
                "created",
                "original_backup",
                "original_sha256",
                "post_sha256",
            ],
        )?;
    }
    let manifest: Manifest = serde_json::from_value(value)
        .map_err(|_| invalid("The backup manifest has invalid fields or client names."))?;
    if manifest.version != 1 {
        return Err(invalid("The backup manifest version is unsupported."));
    }
    for (client, entry) in &manifest.entries {
        if !Path::new(&entry.target).is_absolute() || !valid_hash(&entry.post_sha256) {
            return Err(invalid(
                "The backup manifest contains invalid target metadata.",
            ));
        }
        if entry.created {
            if entry.original_backup.is_some() || entry.original_sha256.is_some() {
                return Err(invalid(
                    "An adapter-created file must not claim an original backup.",
                ));
            }
        } else if entry.original_backup.as_deref() != Some(client.original().as_str())
            || !entry.original_sha256.as_deref().is_some_and(valid_hash)
        {
            return Err(invalid(
                "The backup manifest contains invalid original metadata.",
            ));
        }
    }
    Ok(manifest)
}

pub(super) fn exact_keys(value: &Value, keys: &[&str]) -> Result<()> {
    if !value.as_object().is_some_and(|object| {
        object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key))
    }) {
        return Err(invalid(
            "Configuration metadata has missing or unsupported fields.",
        ));
    }
    Ok(())
}

pub(super) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn endpoint(value: &str) -> Result<String> {
    let reject = || invalid("Use an HTTP loopback root URL with a fixed, nonzero port.");
    if value.chars().any(|character| character.is_ascii_control()) {
        return Err(reject());
    }
    let value = value.trim();
    let (scheme, authority) = value.split_once("://").ok_or_else(reject)?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if !scheme.eq_ignore_ascii_case("http") || authority.contains(['/', '?', '#', '@', '\\']) {
        return Err(reject());
    }
    let (host, port) = if let Some(tail) = authority.strip_prefix("[::1]") {
        (
            "::1",
            if tail.is_empty() {
                None
            } else {
                Some(tail.strip_prefix(':').ok_or_else(reject)?)
            },
        )
    } else {
        authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)))
    };
    if !host.eq_ignore_ascii_case("localhost") && host != "127.0.0.1" && host != "::1" {
        return Err(reject());
    }
    if let Some(port) = port
        && (port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || !port.parse::<u16>().is_ok_and(|port| port != 0))
    {
        return Err(reject());
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn codex(raw: Option<&[u8]>) -> Result<DocumentMut> {
    let text = std::str::from_utf8(raw.unwrap_or_default())
        .map_err(|_| invalid("Codex settings contain invalid UTF-8 TOML."))?;
    let document = text
        .parse::<DocumentMut>()
        .map_err(|_| invalid("Codex settings contain invalid UTF-8 TOML."))?;
    string(&document, "model_provider")?;
    Ok(document)
}

fn string(document: &DocumentMut, key: &str) -> Result<Option<String>> {
    document
        .get(key)
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| invalid("A Codex selection field must be a nonempty string."))
        })
        .transpose()
}

fn integer(document: &DocumentMut, key: &str) -> Result<Option<u64>> {
    document
        .get(key)
        .map(|value| {
            value
                .as_integer()
                .and_then(|value| u64::try_from(value).ok())
                .ok_or_else(|| {
                    invalid("Codex context and compaction values must be nonnegative integers.")
                })
        })
        .transpose()
}

pub(super) fn codex_settings(raw: Option<&[u8]>) -> Result<CodexSettings> {
    let document = codex(raw)?;
    let mut builtin = false;
    if let Some(providers) = document.get("model_providers") {
        let providers = providers
            .as_table_like()
            .ok_or_else(|| invalid("Codex model_providers must be a table."))?;
        if let Some(openai) = providers.get("openai") {
            if openai.as_table_like().is_none() {
                return Err(invalid("Codex model_providers.openai must be a table."));
            }
            builtin = true;
        }
    }
    let mut profile_overrides = Vec::new();
    if let Some(profile) = string(&document, "profile")? {
        let profile = document
            .get("profiles")
            .and_then(Item::as_table_like)
            .and_then(|profiles| profiles.get(&profile))
            .and_then(Item::as_table_like)
            .ok_or_else(|| invalid("Codex profile must name an existing profile table."))?;
        for key in [
            "model",
            "model_provider",
            "openai_base_url",
            "model_reasoning_effort",
            "model_context_window",
            "model_auto_compact_token_limit",
        ] {
            if profile.contains_key(key) {
                profile_overrides.push(key.to_owned());
            }
        }
    }
    Ok(CodexSettings {
        model: string(&document, "model")?,
        reasoning_effort: string(&document, "model_reasoning_effort")?,
        provider: string(&document, "model_provider")?.unwrap_or_else(|| "openai".into()),
        endpoint: string(&document, "openai_base_url")?,
        profile_overrides,
        context_window: integer(&document, "model_context_window")?,
        auto_compact_token_limit: integer(&document, "model_auto_compact_token_limit")?,
        openai_provider_override: builtin,
    })
}

fn claude(raw: Option<&[u8]>) -> Result<Value> {
    let value = raw
        .map(json)
        .transpose()?
        .unwrap_or_else(|| Value::Object(Map::new()));
    if value.get("env").is_some_and(|value| !value.is_object()) {
        return Err(invalid("Claude settings env must be a JSON object."));
    }
    Ok(value)
}

pub(super) fn validate_document(client: Client, raw: Option<&[u8]>) -> Result<()> {
    match client {
        Client::Codex => {
            codex(raw)?;
        }
        Client::Claude => {
            claude(raw)?;
        }
    }
    Ok(())
}

pub(super) fn render(
    client: Client,
    raw: Option<&[u8]>,
    endpoint: &str,
    options: &ConfigureOptions,
) -> Result<Vec<u8>> {
    if client == Client::Claude {
        let mut value = claude(raw)?;
        if value
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(Value::as_str)
            == Some(endpoint)
        {
            return Ok(raw.unwrap_or_default().to_vec());
        }
        let object = value.as_object_mut().expect("validated object");
        let env = object
            .entry("env")
            .or_insert_with(|| Value::Object(Map::new()));
        env.as_object_mut()
            .expect("validated env")
            .insert("ANTHROPIC_BASE_URL".into(), endpoint.into());
        return serialize(&value);
    }
    let mut document = codex(raw)?;
    let provider = string(&document, "model_provider")?.unwrap_or_else(|| "openai".into());
    if provider != "openai" && !options.force_openai_provider {
        return Err(conflict(
            "Codex uses a custom model_provider; explicitly request force_openai_provider to change it.",
        ));
    }
    let mut updates = vec![("openai_base_url", endpoint)];
    if provider != "openai" {
        updates.push(("model_provider", "openai"));
    }
    if let Some(model) = options.model.as_deref() {
        updates.push(("model", model));
    }
    if let Some(effort) = options.reasoning_effort.as_deref() {
        updates.push(("model_reasoning_effort", effort));
    }
    if let Some(raw) = raw
        && updates
            .iter()
            .all(|(key, value)| document.get(key).and_then(Item::as_str) == Some(*value))
    {
        return Ok(raw.to_vec());
    }
    for (key, value) in updates {
        if document.get(key).and_then(Item::as_str) != Some(value) {
            let mut replacement = toml_edit::Value::from(value);
            if let Some(old) = document.get(key).and_then(Item::as_value) {
                *replacement.decor_mut() = old.decor().clone();
            }
            document[key] = Item::Value(replacement);
        }
    }
    let result = document.to_string().into_bytes();
    if result.len() > MAX_BYTES {
        return Err(invalid(
            "Generated Codex settings exceed the 8 MiB safety limit.",
        ));
    }
    Ok(result)
}

pub(super) fn configured(
    client: Client,
    raw: Option<&[u8]>,
    endpoint: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(bool, Vec<String>)> {
    if client == Client::Claude {
        let value = claude(raw)?;
        return Ok((
            value
                .get("env")
                .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
                .and_then(Value::as_str)
                == Some(endpoint),
            Vec::new(),
        ));
    }
    let settings = codex_settings(raw)?;
    let mut issues = Vec::new();
    if settings.openai_provider_override {
        issues.push("A custom model_providers.openai table can override the endpoint; resolve it explicitly.".into());
    }
    if !settings.profile_overrides.is_empty() {
        issues.push("The active Codex profile overrides top-level adapter selections; resolve it explicitly.".into());
    }
    let configured = settings.endpoint.as_deref() == Some(endpoint)
        && settings.provider == "openai"
        && model.is_none_or(|model| settings.model.as_deref() == Some(model))
        && effort.is_none_or(|effort| settings.reasoning_effort.as_deref() == Some(effort))
        && issues.is_empty();
    Ok((configured, issues))
}

pub(super) const ROUTE_KEYS: [&str; 4] = [
    "model_provider",
    "openai_base_url",
    "model",
    "model_reasoning_effort",
];

pub(super) fn route_fields(raw: Option<&[u8]>) -> Result<BTreeMap<String, Option<String>>> {
    let doc = codex(raw)?;
    ROUTE_KEYS
        .into_iter()
        .map(|key| Ok((key.to_string(), string(&doc, key)?)))
        .collect()
}

/// Revert only fields still equal to what this activation wrote. Other edits,
/// including an explicitly changed provider/endpoint, belong to the user.
pub(super) fn restore_route_fields(
    current: Option<&[u8]>,
    original: Option<&[u8]>,
    applied: Option<&BTreeMap<String, Option<String>>>,
) -> Result<Option<Vec<u8>>> {
    let Some(raw) = current else {
        return Ok(None);
    };
    let mut doc = codex(Some(raw))?;
    let old = codex(original)?;
    let mut changed = false;
    for key in ROUTE_KEYS {
        let value = string(&doc, key)?;
        if applied.is_some_and(|expected| expected.get(key) != Some(&value)) {
            continue;
        }
        if value == string(&old, key)? {
            continue;
        }
        match old.get(key) {
            Some(item) => {
                doc[key] = item.clone();
            }
            None => {
                doc.remove(key);
            }
        }
        changed = true;
    }
    if !changed {
        return Ok(Some(raw.to_vec()));
    }
    let result = doc.to_string().into_bytes();
    if result.len() > MAX_BYTES {
        return Err(invalid("Restored settings exceed their byte limit."));
    }
    Ok(Some(result))
}
