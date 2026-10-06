use crate::selection::AccountScope;
use adapter_protocol::{AdapterError, Result, Value};
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct HistoryLimits {
    pub ttl: Duration,
    pub max_entries: usize,
    pub max_bytes: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(1800),
            max_entries: 128,
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(PartialEq)]
pub struct StoredResponse {
    pub scope: AccountScope,
    pub route: String,
    pub request: Value,
    pub response: Value,
}

impl fmt::Debug for StoredResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredResponse")
            .field("scope", &self.scope)
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct HistoryEnvelope<'a> {
    request: &'a Value,
    response: &'a Value,
    route: &'a str,
}

struct Entry {
    value: Arc<StoredResponse>,
    expires_at: Instant,
    serialized_bytes: usize,
}

#[derive(Default)]
struct Entries {
    values: HashMap<String, Entry>,
    lru: VecDeque<String>,
    bytes: usize,
}

impl Entries {
    fn remove(&mut self, id: &str) -> bool {
        if let Some(entry) = self.values.remove(id) {
            self.bytes -= entry.serialized_bytes;
            self.lru.retain(|value| value != id);
            return true;
        }
        false
    }

    fn expire(&mut self, now: Instant) {
        let expired = self
            .values
            .iter()
            .filter_map(|(id, entry)| (now >= entry.expires_at).then_some(id.clone()))
            .collect::<Vec<_>>();
        for id in expired {
            self.remove(&id);
        }
    }

    fn touch(&mut self, id: &str) {
        self.lru.retain(|value| value != id);
        self.lru.push_back(id.to_string());
    }
}

pub struct ResponseHistory {
    scope: AccountScope,
    limits: HistoryLimits,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    entries: Mutex<Entries>,
}

impl ResponseHistory {
    pub fn new(scope: AccountScope, limits: HistoryLimits) -> Result<Self> {
        Self::with_clock(scope, limits, Arc::new(Instant::now))
    }

    pub fn with_clock(
        scope: AccountScope,
        limits: HistoryLimits,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Result<Self> {
        if limits.ttl.is_zero() || limits.max_entries == 0 || limits.max_bytes == 0 {
            return Err(AdapterError::invalid("History limits must be positive."));
        }
        Ok(Self {
            scope,
            limits,
            clock,
            entries: Mutex::new(Entries::default()),
        })
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, Entries>> {
        self.entries.lock().map_err(|_| {
            AdapterError::new(
                500,
                "history_unavailable",
                "Response history is unavailable after an internal failure.",
            )
        })
    }

    pub fn remember(&self, request: Value, response: Value, route: &str) -> Result<bool> {
        let id = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                AdapterError::upstream("Cannot retain a response without an identifier.")
            })?
            .to_string();
        if !matches!(
            response.get("status").and_then(Value::as_str),
            Some("completed" | "incomplete")
        ) || !response.get("output").is_some_and(Value::is_array)
            || route.is_empty()
            || !request.is_object()
        {
            return Err(AdapterError::upstream(
                "Cannot retain an invalid terminal response.",
            ));
        }
        let serialized = serde_json::to_vec(&HistoryEnvelope {
            request: &request,
            response: &response,
            route,
        })
        .map_err(|_| AdapterError::upstream("Cannot serialize response history."))?;
        let size = serialized.len();
        if size > self.limits.max_bytes {
            return Ok(false);
        }
        let value = Arc::new(StoredResponse {
            scope: self.scope.clone(),
            route: route.into(),
            request,
            response,
        });
        let now = (self.clock)();
        let expires_at = now.checked_add(self.limits.ttl).ok_or_else(|| {
            AdapterError::invalid("History TTL exceeds the monotonic clock range.")
        })?;
        let mut entries = self.locked()?;
        entries.expire(now);
        if let Some(previous) = entries.values.get(&id) {
            if previous.value != value {
                return Err(AdapterError::upstream(
                    "Upstream reused a response identifier for a different request.",
                ));
            }
            entries.touch(&id);
            return Ok(true);
        }
        while !entries.values.is_empty()
            && (entries.values.len() >= self.limits.max_entries
                || size > self.limits.max_bytes - entries.bytes)
        {
            let oldest = entries.lru.front().cloned().ok_or_else(|| {
                AdapterError::new(
                    500,
                    "history_invariant",
                    "History eviction state is inconsistent.",
                )
            })?;
            entries.remove(&oldest);
        }
        entries.bytes += size;
        entries.values.insert(
            id.clone(),
            Entry {
                value,
                expires_at,
                serialized_bytes: size,
            },
        );
        entries.touch(&id);
        Ok(true)
    }

    pub fn read(&self, id: &str) -> Result<Arc<StoredResponse>> {
        if id.is_empty() {
            return Err(AdapterError::invalid(
                "previous_response_id must be a nonempty string.",
            ));
        }
        let mut entries = self.locked()?;
        entries.expire((self.clock)());
        let value = entries.values.get(id).map(|entry| Arc::clone(&entry.value))
            .ok_or_else(|| AdapterError::new(
                404, "history_unavailable",
                "Response history is unknown, expired, or evicted in this provider/account. Resend full input.",
            ))?;
        entries.touch(id);
        Ok(value)
    }

    pub fn expand(&self, request: &Value, route: &str, model: &str) -> Result<Value> {
        let mut expanded = request
            .as_object()
            .ok_or_else(|| AdapterError::invalid("Responses request must be an object."))?
            .clone();
        let previous = expanded.remove("previous_response_id");
        let Some(previous) = previous.filter(|value| !value.is_null()) else {
            return Ok(Value::Object(expanded));
        };
        let previous = previous
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AdapterError::invalid("previous_response_id must be a nonempty string.")
            })?;
        let record = self.read(previous)?;
        if record.scope != self.scope
            || record.route != route
            || record.request.get("model").and_then(Value::as_str) != Some(model)
        {
            return Err(AdapterError::invalid(
                "Response history belongs to a different model or protocol route. Start a new conversation.",
            ));
        }
        let mut input = input_items(record.request.get("input"))?;
        input.extend(
            record.response["output"]
                .as_array()
                .ok_or_else(|| AdapterError::upstream("Retained response has invalid output."))?
                .iter()
                .cloned(),
        );
        input.extend(input_items(expanded.get("input"))?);
        expanded.insert("input".into(), Value::Array(input));
        Ok(Value::Object(expanded))
    }

    pub fn discard(&self, id: &str) -> Result<bool> {
        let mut entries = self.locked()?;
        entries.expire((self.clock)());
        Ok(entries.remove(id))
    }

    pub fn statistics(&self) -> Result<Value> {
        let mut entries = self.locked()?;
        entries.expire((self.clock)());
        Ok(json!({
            "entries": entries.values.len(), "serialized_bytes": entries.bytes,
            "max_entries": self.limits.max_entries, "max_bytes": self.limits.max_bytes,
            "ttl_seconds": self.limits.ttl.as_secs_f64(),
        }))
    }
}

pub fn input_items(value: Option<&Value>) -> Result<Vec<Value>> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::String(value)) => Ok(vec![json!({"role": "user", "content": value})]),
        Some(Value::Array(items)) if items.iter().all(Value::is_object) => Ok(items.clone()),
        _ => Err(AdapterError::invalid(
            "input must be a string or an array of objects.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Provider;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn record(id: &str) -> Value {
        json!({"id": id, "status": "completed", "output": [
            {"id": "item", "type": "reasoning", "encrypted_content": "synthetic-state"}
        ]})
    }

    fn store(limits: HistoryLimits) -> ResponseHistory {
        ResponseHistory::new(
            AccountScope::new(Provider::Github, "fixture").unwrap(),
            limits,
        )
        .unwrap()
    }

    #[test]
    fn continuation_retains_output_but_does_not_inherit_instructions() {
        let history = store(HistoryLimits::default());
        history
            .remember(
                json!({"model": "fixture", "input": "first", "instructions": "old"}),
                record("one"),
                "/responses",
            )
            .unwrap();
        let expanded = history
            .expand(
                &json!({
                    "model": "fixture", "previous_response_id": "one", "input": "second",
                }),
                "/responses",
                "fixture",
            )
            .unwrap();
        assert_eq!(expanded["input"].as_array().unwrap().len(), 3);
        assert_eq!(expanded["input"][1]["encrypted_content"], "synthetic-state");
        assert!(expanded.get("instructions").is_none());
        assert!(expanded.get("previous_response_id").is_none());
        assert!(
            history
                .expand(
                    &json!({"previous_response_id": "one"}),
                    "/chat/completions",
                    "fixture"
                )
                .is_err()
        );
    }

    #[test]
    fn reads_change_lru_but_not_ttl_and_ancestors_are_not_required_by_children() {
        let start = Instant::now();
        let seconds = Arc::new(AtomicU64::new(0));
        let clock_seconds = Arc::clone(&seconds);
        let history = ResponseHistory::with_clock(
            AccountScope::new(Provider::Github, "fixture").unwrap(),
            HistoryLimits::default(),
            Arc::new(move || start + Duration::from_secs(clock_seconds.load(Ordering::SeqCst))),
        )
        .unwrap();
        history
            .remember(
                json!({"model": "fixture", "input": "first"}),
                record("one"),
                "/responses",
            )
            .unwrap();
        let second = history
            .expand(
                &json!({"previous_response_id": "one", "input": "next"}),
                "/responses",
                "fixture",
            )
            .unwrap();
        history
            .remember(
                json!({"model": "fixture", "input": second["input"]}),
                record("two"),
                "/responses",
            )
            .unwrap();
        history.discard("one").unwrap();
        assert!(
            history
                .expand(
                    &json!({"previous_response_id": "two", "input": "last"}),
                    "/responses",
                    "fixture"
                )
                .is_ok()
        );
        seconds.store(1799, Ordering::SeqCst);
        assert!(history.read("two").is_ok());
        seconds.store(1800, Ordering::SeqCst);
        assert_eq!(history.read("two").unwrap_err().status, 404);
    }

    #[test]
    fn limits_and_identifier_collisions_are_explicit() {
        let history = store(HistoryLimits {
            max_entries: 1,
            ..HistoryLimits::default()
        });
        let request = json!({"model": "fixture", "input": "one"});
        history
            .remember(request.clone(), record("one"), "/responses")
            .unwrap();
        assert!(
            history
                .remember(json!({"model": "different"}), record("one"), "/responses")
                .is_err()
        );
        history
            .remember(request, record("two"), "/responses")
            .unwrap();
        assert_eq!(history.read("one").unwrap_err().status, 404);
        assert_eq!(history.statistics().unwrap()["entries"], 1);
        let tiny = store(HistoryLimits {
            max_bytes: 1,
            ..HistoryLimits::default()
        });
        assert!(
            !tiny
                .remember(json!({"input": "large"}), record("one"), "/responses")
                .unwrap()
        );
        assert_eq!(tiny.statistics().unwrap()["entries"], 0);
    }
}
