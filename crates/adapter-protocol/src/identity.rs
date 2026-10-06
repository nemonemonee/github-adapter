use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Map, Value};

use crate::error::{AdapterError, Result};
use crate::json::nonnegative_integer;
use crate::sse::{SseEvent, StreamLimits};

#[derive(Debug, Default)]
struct TextState {
    text: String,
    observed: bool,
    closed: bool,
}

impl TextState {
    fn initial(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.observed && self.text != text {
            return Err(inconsistent_text());
        }
        if !self.observed {
            self.text.push_str(text);
            self.observed = true;
        }
        Ok(())
    }

    fn delta(&mut self, text: &str) -> Result<()> {
        if self.closed {
            return Err(AdapterError::upstream(
                "Copilot sent a delta after finalizing its content.",
            ));
        }
        // An empty placeholder is not an observation of a final empty value.
        if !text.is_empty() {
            self.text.push_str(text);
            self.observed = true;
        }
        Ok(())
    }

    fn complete(&mut self, text: &str) -> Result<()> {
        if self.observed && self.text != text {
            return Err(inconsistent_text());
        }
        if !self.observed {
            self.text.push_str(text);
        }
        self.observed = true;
        self.closed = true;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct PartState {
    kind: Option<String>,
    text: TextState,
}

#[derive(Debug, Default)]
struct ItemState {
    id: Option<String>,
    kind: Option<String>,
    phase: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: TextState,
    content: BTreeMap<usize, PartState>,
    summary: BTreeMap<usize, PartState>,
    closed: bool,
}

#[derive(Clone, Copy)]
enum Domain {
    Content,
    Summary,
}

#[derive(Clone, Copy)]
enum TextKind {
    Output,
    Refusal,
    Summary,
    Reasoning,
}

#[derive(Clone, Copy)]
enum Snapshot {
    Initial,
    ItemDone,
    Terminal { completed: bool },
}

impl Snapshot {
    fn is_final(self) -> bool {
        !matches!(self, Self::Initial)
    }
}

/// Per-request native Responses validation. Only response/item identity fields
/// are rewritten; opaque content, call IDs, phases and extension fields survive.
///
/// Error events return the upstream failure before validating its resource
/// identities. The authenticated caller still owns credential redaction.
#[derive(Debug)]
pub struct NativeStream {
    limits: StreamLimits,
    response_id: Option<String>,
    response_aliases: HashSet<String>,
    items: BTreeMap<usize, ItemState>,
    aliases: HashMap<String, usize>,
    call_owners: HashMap<String, usize>,
    part_count: usize,
    received_bytes: usize,
    emitted_bytes: usize,
    terminal: bool,
    done: bool,
    finished: bool,
    failure: Option<AdapterError>,
}

impl Default for NativeStream {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeStream {
    pub fn new() -> Self {
        Self::with_limits(StreamLimits::default())
    }

    pub fn with_limits(limits: StreamLimits) -> Self {
        Self {
            limits,
            response_id: None,
            response_aliases: HashSet::new(),
            items: BTreeMap::new(),
            aliases: HashMap::new(),
            call_owners: HashMap::new(),
            part_count: 0,
            received_bytes: 0,
            emitted_bytes: 0,
            terminal: false,
            done: false,
            finished: false,
            failure: None,
        }
    }

    pub fn push(&mut self, event: SseEvent) -> Result<SseEvent> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.push_inner(event);
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }

    pub fn finish(&mut self) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if !self.terminal {
            let error = AdapterError::upstream(
                "Copilot closed the stream before a terminal response was received.",
            );
            self.failure = Some(error.clone());
            return Err(error);
        }
        self.finished = true;
        Ok(())
    }

    fn push_inner(&mut self, mut event: SseEvent) -> Result<SseEvent> {
        if self.finished {
            return Err(AdapterError::upstream(
                "The native stream has already finished.",
            ));
        }
        let incoming = event.encoded_len(self.limits.max_event_bytes)?;
        self.received_bytes =
            bounded_total(self.received_bytes, incoming, self.limits.max_stream_bytes)?;
        if event.kind.is_empty()
            && let Some(kind) = event
                .data
                .as_ref()
                .and_then(|data| data.get("type"))
                .and_then(Value::as_str)
        {
            event.kind = kind.to_owned();
        }
        if event.done {
            if !self.terminal || self.done {
                return Err(AdapterError::upstream(
                    "Copilot ended the native stream without exactly one terminal response.",
                ));
            }
            self.done = true;
        } else if let Some(data) = &mut event.data {
            if matches!(event.kind.as_str(), "error" | "response.failed") {
                return Err(upstream_failure(data));
            }
            if self.terminal {
                return Err(AdapterError::upstream(
                    "Copilot sent an event after the terminal response.",
                ));
            }
            if event.kind.starts_with("response.") {
                let data = data.as_object_mut().ok_or_else(|| {
                    AdapterError::upstream("Copilot returned an invalid native event.")
                })?;
                self.normalize(&event.kind, data)?;
            }
        }
        let outgoing = event.encoded_len(self.limits.max_event_bytes)?;
        self.emitted_bytes =
            bounded_total(self.emitted_bytes, outgoing, self.limits.max_stream_bytes)?;
        Ok(event)
    }

    fn normalize(&mut self, kind: &str, data: &mut Map<String, Value>) -> Result<()> {
        for key in [
            "output_index",
            "content_index",
            "summary_index",
            "annotation_index",
        ] {
            if let Some(value) = data.get(key) {
                index(Some(value), self.limits.max_items)?;
            }
        }
        for key in ["sequence_number", "created_at"] {
            if let Some(value) = data.get(key) {
                unsigned(Some(value))?;
            }
        }
        optional_string(data.get("phase"), true, true)?;

        let terminal = match kind {
            "response.completed" => Some(true),
            "response.incomplete" => Some(false),
            _ => None,
        };
        let terminal_len = match data.get_mut("response") {
            Some(resource) => self.resource(resource, terminal)?,
            None if terminal.is_some() => {
                return Err(AdapterError::upstream(
                    "Copilot omitted its terminal Responses resource.",
                ));
            }
            None => None,
        };
        if let Some(value) = data.get("response_id") {
            let id = self.response_identity(value)?;
            data.insert("response_id".into(), Value::String(id));
        }

        if data.contains_key("item") || data.contains_key("item_id") {
            let output_index = index(data.get("output_index"), self.limits.max_items)?;
            self.ensure_item(output_index)?;
            if let Some(item) = data.get_mut("item") {
                if kind == "response.output_item.added" && self.items[&output_index].closed {
                    return Err(AdapterError::upstream(
                        "Copilot announced an already finalized output item.",
                    ));
                }
                self.item(
                    output_index,
                    item,
                    if kind == "response.output_item.done" {
                        Snapshot::ItemDone
                    } else {
                        Snapshot::Initial
                    },
                    true,
                )?;
            }
            if let Some(value) = data.get("item_id") {
                let id = self.item_identity(output_index, value)?;
                data.insert("item_id".into(), Value::String(id));
            }
        }
        match kind {
            "response.output_item.added" | "response.output_item.done" => {
                if !data.contains_key("item") {
                    return Err(AdapterError::upstream(
                        "Copilot omitted a streamed output item.",
                    ));
                }
            }
            "response.output_text.delta" | "response.output_text.done" => {
                self.text_event(data, kind.ends_with(".done"), TextKind::Output)?;
            }
            "response.refusal.delta" | "response.refusal.done" => {
                self.text_event(data, kind.ends_with(".done"), TextKind::Refusal)?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_summary_text.done" => {
                self.text_event(data, kind.ends_with(".done"), TextKind::Summary)?;
            }
            "response.reasoning_text.delta" | "response.reasoning_text.done" => {
                self.text_event(data, kind.ends_with(".done"), TextKind::Reasoning)?;
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                self.arguments_event(data, kind.ends_with(".done"))?;
            }
            "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {
                let output_index = self.event_item(data)?;
                let domain = if kind.starts_with("response.reasoning_summary_part.") {
                    Domain::Summary
                } else {
                    Domain::Content
                };
                let part_index = index(data.get(domain.key()), self.limits.max_items)?;
                if kind.ends_with(".added")
                    && (self.items[&output_index].closed
                        || domain
                            .parts(&self.items[&output_index])
                            .get(&part_index)
                            .is_some_and(|part| part.text.closed))
                {
                    return Err(AdapterError::upstream(
                        "Copilot announced an already finalized content part.",
                    ));
                }
                let part = data.get("part").ok_or_else(|| {
                    AdapterError::upstream("Copilot omitted a streamed content part.")
                })?;
                self.part(
                    output_index,
                    domain,
                    part_index,
                    part,
                    kind.ends_with(".done"),
                )?;
            }
            "response.output_text.annotation.added" => {
                let output_index = self.event_item(data)?;
                let part_index = index(data.get("content_index"), self.limits.max_items)?;
                index(data.get("annotation_index"), self.limits.max_items)?;
                if !data.get("annotation").is_some_and(Value::is_object) {
                    return Err(AdapterError::upstream(
                        "Copilot returned an invalid annotation.",
                    ));
                }
                self.bind_item_kind(output_index, "message")?;
                let part = self.ensure_part(output_index, Domain::Content, part_index)?;
                bind(
                    &mut part.kind,
                    "output_text",
                    "Copilot changed a native content part type.",
                )?;
            }
            _ => {}
        }
        if let Some(length) = terminal_len {
            self.covered_outputs(length)?;
            self.terminal = true;
        }
        Ok(())
    }

    fn resource(&mut self, value: &mut Value, terminal: Option<bool>) -> Result<Option<usize>> {
        if terminal.is_some() && value.get("error").is_some_and(|error| !error.is_null()) {
            return Err(upstream_failure(value));
        }
        let resource = value.as_object_mut().ok_or_else(|| {
            AdapterError::upstream("Copilot returned an invalid Responses envelope.")
        })?;
        if let Some(value) = resource.get("id") {
            let id = self.response_identity(value)?;
            resource.insert("id".into(), Value::String(id));
        } else if terminal.is_some() {
            return Err(AdapterError::upstream(
                "Copilot omitted its response identifier.",
            ));
        }
        if let Some(completed) = terminal {
            let expected = if completed { "completed" } else { "incomplete" };
            if resource.get("status").and_then(Value::as_str) != Some(expected) {
                return Err(AdapterError::upstream(
                    "Copilot returned an invalid terminal status.",
                ));
            }
        } else {
            optional_string(resource.get("status"), true, false)?;
        }
        if let Some(created) = resource.get("created_at") {
            unsigned(Some(created))?;
        }
        if let Some(usage) = resource.get("usage").filter(|usage| !usage.is_null()) {
            let usage = usage
                .as_object()
                .ok_or_else(|| AdapterError::upstream("Copilot returned invalid native usage."))?;
            for key in ["input_tokens", "output_tokens", "total_tokens"] {
                if let Some(count) = usage.get(key) {
                    unsigned(Some(count))?;
                }
            }
        }
        let output = match resource.get_mut("output") {
            Some(value) => value.as_array_mut().ok_or_else(|| {
                AdapterError::upstream("Copilot returned an invalid Responses output array.")
            })?,
            None if terminal.is_some() => {
                return Err(AdapterError::upstream(
                    "Copilot omitted its terminal output array.",
                ));
            }
            None => return Ok(None),
        };
        if output.len() > self.limits.max_items {
            return Err(item_limit());
        }
        if terminal.is_some() {
            self.covered_outputs(output.len())?;
        }
        for (index, item) in output.iter_mut().enumerate() {
            self.item(
                index,
                item,
                terminal.map_or(Snapshot::Initial, |completed| Snapshot::Terminal {
                    completed,
                }),
                false,
            )?;
        }
        Ok(terminal.map(|_| output.len()))
    }

    fn response_identity(&mut self, value: &Value) -> Result<String> {
        let id = identifier(value)?;
        self.response_aliases.insert(id.to_owned());
        Ok(self
            .response_id
            .get_or_insert_with(|| id.to_owned())
            .clone())
    }

    fn item_identity(&mut self, output_index: usize, value: &Value) -> Result<String> {
        let id = identifier(value)?;
        self.ensure_item(output_index)?;
        if self
            .aliases
            .get(id)
            .is_some_and(|owner| *owner != output_index)
        {
            return Err(AdapterError::upstream(
                "Copilot reused an item identifier across output indexes.",
            ));
        }
        self.aliases.insert(id.to_owned(), output_index);
        Ok(self
            .items
            .get_mut(&output_index)
            .expect("item exists")
            .id
            .get_or_insert_with(|| id.to_owned())
            .clone())
    }

    fn ensure_item(&mut self, index: usize) -> Result<()> {
        if index >= self.limits.max_items {
            return Err(item_limit());
        }
        self.items.entry(index).or_default();
        Ok(())
    }

    fn bind_item_kind(&mut self, index: usize, kind: &str) -> Result<()> {
        self.ensure_item(index)?;
        bind(
            &mut self.items.get_mut(&index).expect("item exists").kind,
            kind,
            "Copilot changed the item type at an existing output_index.",
        )
    }

    fn item(
        &mut self,
        index: usize,
        value: &mut Value,
        snapshot: Snapshot,
        require_id: bool,
    ) -> Result<()> {
        let item = value.as_object_mut().ok_or_else(|| {
            AdapterError::upstream("Copilot returned an invalid native output item.")
        })?;
        self.ensure_item(index)?;
        if let Some(kind) = optional_string(item.get("type"), true, false)? {
            self.bind_item_kind(index, kind)?;
        } else if snapshot.is_final() {
            return Err(AdapterError::upstream(
                "Copilot omitted a finalized output item type.",
            ));
        }
        if let Some(value) = item.get("id") {
            let id = self.item_identity(index, value)?;
            item.insert("id".into(), Value::String(id));
        } else if let Some(id) = &self.items[&index].id {
            item.insert("id".into(), Value::String(id.clone()));
        } else if require_id {
            return Err(AdapterError::upstream(
                "Copilot omitted the identifier for a streamed output item.",
            ));
        }
        let phase = optional_string(item.get("phase"), true, true)?;
        let state = self.items.get_mut(&index).expect("item exists");
        if let Some(phase) = phase {
            bind(
                &mut state.phase,
                phase,
                "Copilot changed a native message phase.",
            )?;
        } else if snapshot.is_final() && state.phase.is_some() {
            return Err(AdapterError::upstream(
                "Copilot omitted an observed message phase.",
            ));
        }
        let kind = state.kind.clone();
        let status = optional_string(item.get("status"), true, false)?;
        if let Some(status) = status
            && matches!(
                kind.as_deref(),
                Some("message" | "function_call" | "reasoning")
            )
            && (!matches!(status, "in_progress" | "completed" | "incomplete")
                || (snapshot.is_final() && status == "in_progress")
                || (matches!(snapshot, Snapshot::Terminal { completed: true })
                    && status != "completed"))
        {
            return Err(AdapterError::upstream(
                "Copilot returned an inconsistent output item status.",
            ));
        }
        match kind.as_deref() {
            Some("message") => {
                if item
                    .get("role")
                    .is_some_and(|role| role.as_str() != Some("assistant"))
                {
                    return Err(AdapterError::upstream(
                        "Copilot returned an invalid output message role.",
                    ));
                }
                self.parts(
                    index,
                    Domain::Content,
                    item.get("content"),
                    snapshot.is_final(),
                    true,
                )?;
            }
            Some("reasoning") => {
                optional_string(item.get("encrypted_content"), false, true)?;
                self.parts(
                    index,
                    Domain::Summary,
                    item.get("summary"),
                    snapshot.is_final(),
                    false,
                )?;
                self.parts(
                    index,
                    Domain::Content,
                    item.get("content"),
                    snapshot.is_final(),
                    false,
                )?;
            }
            Some("function_call") => {
                self.call_fields(index, item, snapshot.is_final())?;
                let arguments = optional_string(item.get("arguments"), false, false)?;
                match arguments {
                    Some(arguments) => {
                        let state = &mut self.items.get_mut(&index).expect("item exists").arguments;
                        if snapshot.is_final() {
                            state.complete(arguments)?;
                        } else {
                            state.initial(arguments)?;
                        }
                    }
                    None if snapshot.is_final() => {
                        return Err(AdapterError::upstream(
                            "Copilot omitted native function arguments.",
                        ));
                    }
                    None => {}
                }
            }
            Some("compaction") => {
                let content = optional_string(item.get("encrypted_content"), true, false)?;
                if snapshot.is_final() && content.is_none_or(|content| content.trim().is_empty()) {
                    return Err(AdapterError::upstream(
                        "Copilot omitted genuine encrypted compaction content.",
                    ));
                }
            }
            _ => {}
        }
        if snapshot.is_final() {
            self.items.get_mut(&index).expect("item exists").closed = true;
        }
        Ok(())
    }

    fn call_fields(
        &mut self,
        index: usize,
        fields: &Map<String, Value>,
        required: bool,
    ) -> Result<()> {
        let call_id = optional_string(fields.get("call_id"), true, false)?;
        let name = optional_string(fields.get("name"), true, false)?;
        if required && (call_id.is_none() || name.is_none()) {
            return Err(AdapterError::upstream(
                "Copilot omitted native function call identity.",
            ));
        }
        if let Some(call_id) = call_id {
            if self
                .call_owners
                .get(call_id)
                .is_some_and(|owner| *owner != index)
            {
                return Err(AdapterError::upstream(
                    "Copilot reused a function call identifier.",
                ));
            }
            self.call_owners.insert(call_id.to_owned(), index);
            bind(
                &mut self.items.get_mut(&index).expect("item exists").call_id,
                call_id,
                "Copilot changed a native function call identifier.",
            )?;
        }
        if let Some(name) = name {
            bind(
                &mut self.items.get_mut(&index).expect("item exists").name,
                name,
                "Copilot changed a native function name.",
            )?;
        }
        Ok(())
    }

    fn event_item(&mut self, fields: &Map<String, Value>) -> Result<usize> {
        let output_index = index(fields.get("output_index"), self.limits.max_items)?;
        identifier(fields.get("item_id").ok_or_else(|| {
            AdapterError::upstream("Copilot omitted a streamed item reference.")
        })?)?;
        self.ensure_item(output_index)?;
        Ok(output_index)
    }

    fn text_event(
        &mut self,
        fields: &Map<String, Value>,
        done: bool,
        kind: TextKind,
    ) -> Result<()> {
        let (domain, item_kind, part_kind, final_field) = match kind {
            TextKind::Output => (Domain::Content, "message", "output_text", "text"),
            TextKind::Refusal => (Domain::Content, "message", "refusal", "refusal"),
            TextKind::Summary => (Domain::Summary, "reasoning", "summary_text", "text"),
            TextKind::Reasoning => (Domain::Content, "reasoning", "reasoning_text", "text"),
        };
        let index = self.event_item(fields)?;
        if !done && self.items[&index].closed {
            return Err(AdapterError::upstream(
                "Copilot sent a delta after finalizing its output item.",
            ));
        }
        self.bind_item_kind(index, item_kind)?;
        let part_index = index_value(fields.get(domain.key()), self.limits.max_items)?;
        let text = required_string(fields.get(if done { final_field } else { "delta" }), false)?;
        let part = self.ensure_part(index, domain, part_index)?;
        bind(
            &mut part.kind,
            part_kind,
            "Copilot changed a native content part type.",
        )?;
        if done {
            part.text.complete(text)
        } else {
            part.text.delta(text)
        }
    }

    fn arguments_event(&mut self, fields: &Map<String, Value>, done: bool) -> Result<()> {
        let index = self.event_item(fields)?;
        self.bind_item_kind(index, "function_call")?;
        if !done && self.items[&index].closed {
            return Err(AdapterError::upstream(
                "Copilot sent arguments after finalizing its function call.",
            ));
        }
        self.call_fields(index, fields, false)?;
        let text = required_string(fields.get(if done { "arguments" } else { "delta" }), false)?;
        let arguments = &mut self.items.get_mut(&index).expect("item exists").arguments;
        if done {
            arguments.complete(text)
        } else {
            arguments.delta(text)
        }
    }

    fn ensure_part(
        &mut self,
        index: usize,
        domain: Domain,
        part_index: usize,
    ) -> Result<&mut PartState> {
        self.ensure_item(index)?;
        if part_index >= self.limits.max_items {
            return Err(item_limit());
        }
        let state = self.items.get_mut(&index).expect("item exists");
        let closed = state.closed;
        let parts = domain.parts_mut(state);
        if !parts.contains_key(&part_index) {
            if closed {
                return Err(AdapterError::upstream(
                    "Copilot added content to an already finalized output item.",
                ));
            }
            if self.part_count >= self.limits.max_items {
                return Err(item_limit());
            }
            self.part_count += 1;
        }
        Ok(parts.entry(part_index).or_default())
    }

    fn parts(
        &mut self,
        index: usize,
        domain: Domain,
        value: Option<&Value>,
        final_snapshot: bool,
        required: bool,
    ) -> Result<()> {
        let parts = match value {
            Some(value) => value.as_array().ok_or_else(|| {
                AdapterError::upstream("Copilot returned an invalid native content array.")
            })?,
            None => {
                if final_snapshot && (required || !domain.parts(&self.items[&index]).is_empty()) {
                    return Err(AdapterError::upstream(
                        "Copilot omitted observed native content.",
                    ));
                }
                return Ok(());
            }
        };
        if parts.len() > self.limits.max_items {
            return Err(item_limit());
        }
        if final_snapshot
            && domain
                .parts(&self.items[&index])
                .last_key_value()
                .is_some_and(|(index, _)| *index >= parts.len())
        {
            return Err(AdapterError::upstream(
                "Copilot omitted an observed native content part.",
            ));
        }
        for (part_index, part) in parts.iter().enumerate() {
            self.part(index, domain, part_index, part, final_snapshot)?;
        }
        Ok(())
    }

    fn part(
        &mut self,
        index: usize,
        domain: Domain,
        part_index: usize,
        value: &Value,
        final_snapshot: bool,
    ) -> Result<()> {
        let fields = value.as_object().ok_or_else(|| {
            AdapterError::upstream("Copilot returned an invalid native content part.")
        })?;
        let kind = required_string(fields.get("type"), true)?;
        match kind {
            "output_text" | "refusal" => self.bind_item_kind(index, "message")?,
            "summary_text" | "reasoning_text" => self.bind_item_kind(index, "reasoning")?,
            _ => {}
        }
        let part = self.ensure_part(index, domain, part_index)?;
        bind(
            &mut part.kind,
            kind,
            "Copilot changed a native content part type.",
        )?;
        let field = match kind {
            "output_text" | "summary_text" | "reasoning_text" => "text",
            "refusal" => "refusal",
            _ => return Ok(()),
        };
        let text = required_string(fields.get(field), false)?;
        if final_snapshot {
            part.text.complete(text)
        } else {
            part.text.initial(text)
        }
    }

    fn covered_outputs(&self, length: usize) -> Result<()> {
        if self
            .items
            .last_key_value()
            .is_some_and(|(index, _)| *index >= length)
        {
            return Err(AdapterError::upstream(
                "Copilot omitted an observed output item from its terminal response.",
            ));
        }
        Ok(())
    }
}

impl Domain {
    fn key(self) -> &'static str {
        match self {
            Self::Content => "content_index",
            Self::Summary => "summary_index",
        }
    }

    fn parts(self, state: &ItemState) -> &BTreeMap<usize, PartState> {
        match self {
            Self::Content => &state.content,
            Self::Summary => &state.summary,
        }
    }

    fn parts_mut(self, state: &mut ItemState) -> &mut BTreeMap<usize, PartState> {
        match self {
            Self::Content => &mut state.content,
            Self::Summary => &mut state.summary,
        }
    }
}

fn bind(slot: &mut Option<String>, value: &str, message: &'static str) -> Result<()> {
    match slot {
        Some(previous) if previous.as_str() != value => Err(AdapterError::upstream(message)),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value.to_owned());
            Ok(())
        }
    }
}

fn identifier(value: &Value) -> Result<&str> {
    required_string(Some(value), true)
}

fn optional_string(value: Option<&Value>, nonempty: bool, nullable: bool) -> Result<Option<&str>> {
    match value {
        None => Ok(None),
        Some(Value::Null) if nullable => Ok(None),
        Some(value) => required_string(Some(value), nonempty).map(Some),
    }
}

fn required_string(value: Option<&Value>, nonempty: bool) -> Result<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !nonempty || !value.is_empty())
        .ok_or_else(|| AdapterError::upstream("Copilot returned an invalid native string field."))
}

fn unsigned(value: Option<&Value>) -> Result<u64> {
    value.and_then(nonnegative_integer).ok_or_else(|| {
        AdapterError::upstream("Copilot returned an invalid nonnegative integer field.")
    })
}

fn index(value: Option<&Value>, limit: usize) -> Result<usize> {
    index_value(value, limit)
}

fn index_value(value: Option<&Value>, limit: usize) -> Result<usize> {
    usize::try_from(unsigned(value)?)
        .ok()
        .filter(|value| *value < limit)
        .ok_or_else(item_limit)
}

fn bounded_total(previous: usize, bytes: usize, limit: usize) -> Result<usize> {
    previous
        .checked_add(bytes)
        .filter(|value| *value <= limit)
        .ok_or_else(|| {
            AdapterError::upstream("Copilot exceeded the cumulative native stream byte limit.")
        })
}

fn inconsistent_text() -> AdapterError {
    AdapterError::upstream("Copilot returned conflicting native content or function arguments.")
}

fn item_limit() -> AdapterError {
    AdapterError::upstream("Copilot exceeded the native item or content index limit.")
}

fn upstream_failure(value: &Value) -> AdapterError {
    let failure = value
        .get("response")
        .filter(|response| response.is_object())
        .unwrap_or(value);
    let message = if let Some(error) = failure.get("error").filter(|error| error.is_object()) {
        error.get("message")
    } else {
        failure.get("message")
    };
    AdapterError::upstream(
        message
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or("The Copilot response failed."),
    )
}
