use std::io::{self, Write};

use serde_json::Value;

use crate::error::{AdapterError, Result};
use crate::json::{self, ByteLimit};

pub const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SSE_STREAM_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_NATIVE_ITEMS: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamLimits {
    pub max_event_bytes: usize,
    pub max_stream_bytes: usize,
    /// Maximum output slots and maximum total content/summary slots in NativeStream.
    /// The framing decoder does not interpret output items or count token events.
    pub max_items: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_event_bytes: MAX_SSE_EVENT_BYTES,
            max_stream_bytes: MAX_SSE_STREAM_BYTES,
            max_items: MAX_NATIVE_ITEMS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SseEvent {
    pub kind: String,
    pub data: Option<Value>,
    pub comment: Option<String>,
    pub done: bool,
}

impl SseEvent {
    pub fn json(kind: impl Into<String>, data: Value) -> Self {
        Self {
            kind: kind.into(),
            data: Some(data),
            ..Self::default()
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let size = self.encoded_len(MAX_SSE_EVENT_BYTES)?;
        let mut bytes = Vec::with_capacity(size);
        self.write_frame(&mut bytes).map_err(|_| frame_error())?;
        Ok(bytes)
    }

    pub(crate) fn encoded_len(&self, max_bytes: usize) -> Result<usize> {
        self.validate()?;
        let mut counter = ByteLimit::new(io::sink(), max_bytes);
        self.write_frame(&mut counter).map_err(|_| {
            AdapterError::upstream("The encoded streaming event exceeds the byte limit.")
        })?;
        Ok(counter.len())
    }

    fn validate(&self) -> Result<()> {
        if invalid_kind(&self.kind) {
            return Err(AdapterError::upstream(
                "Copilot returned an invalid event type.",
            ));
        }
        if (self.done && (self.data.is_some() || self.comment.is_some() || !self.kind.is_empty()))
            || (self.comment.is_some() && (self.data.is_some() || !self.kind.is_empty()))
            || (self.data.is_none() && !self.kind.is_empty())
        {
            return Err(frame_error());
        }
        if let Some(data) = &self.data {
            let object = data.as_object().ok_or_else(|| {
                AdapterError::upstream("Copilot returned an invalid streaming event.")
            })?;
            if let Some(kind) = object.get("type") {
                let kind = kind.as_str().ok_or_else(|| {
                    AdapterError::upstream("Copilot returned an invalid event type.")
                })?;
                if invalid_kind(kind) || (!self.kind.is_empty() && self.kind != kind) {
                    return Err(AdapterError::upstream(
                        "Copilot returned inconsistent streaming event types.",
                    ));
                }
            }
            json::validate_value(data).map_err(|error| AdapterError::upstream(error.message))?;
        }
        Ok(())
    }

    fn write_frame(&self, writer: &mut impl Write) -> io::Result<()> {
        if self.done {
            return writer.write_all(b"data: [DONE]\n\n");
        }
        if let Some(comment) = &self.comment {
            for line in comment.split(['\r', '\n']) {
                writer.write_all(b": ")?;
                writer.write_all(line.as_bytes())?;
                writer.write_all(b"\n")?;
            }
            return writer.write_all(b"\n");
        }
        if let Some(data) = &self.data {
            if !self.kind.is_empty() {
                writer.write_all(b"event: ")?;
                writer.write_all(self.kind.as_bytes())?;
                writer.write_all(b"\n")?;
            }
            writer.write_all(b"data: ")?;
            serde_json::to_writer(&mut *writer, data).map_err(io::Error::other)?;
            writer.write_all(b"\n")?;
        }
        writer.write_all(b"\n")
    }
}

fn invalid_kind(kind: &str) -> bool {
    kind.contains(['\r', '\n'])
}

fn frame_error() -> AdapterError {
    AdapterError::upstream("The streaming event cannot be encoded as an SSE frame.")
}

#[derive(Debug)]
pub struct SseDecoder {
    limits: StreamLimits,
    line: Vec<u8>,
    data: String,
    event_type: String,
    has_data: bool,
    pending_cr: bool,
    first_line: bool,
    event_bytes: usize,
    stream_bytes: usize,
    finished: bool,
    failure: Option<AdapterError>,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new(StreamLimits::default())
    }
}

impl SseDecoder {
    pub fn new(limits: StreamLimits) -> Self {
        Self {
            limits,
            line: Vec::new(),
            data: String::new(),
            event_type: String::new(),
            has_data: false,
            pending_cr: false,
            first_line: true,
            event_bytes: 0,
            stream_bytes: 0,
            finished: false,
            failure: None,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>> {
        self.check_active()?;
        let result = self.push_inner(bytes);
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }

    pub fn finish(&mut self) -> Result<Vec<SseEvent>> {
        self.check_active()?;
        let result = self.finish_inner();
        self.finished = true;
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }

    fn check_active(&self) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.finished {
            return Err(AdapterError::upstream(
                "The SSE decoder has already finished.",
            ));
        }
        Ok(())
    }

    fn push_inner(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>> {
        self.stream_bytes = self
            .stream_bytes
            .checked_add(bytes.len())
            .filter(|size| *size <= self.limits.max_stream_bytes)
            .ok_or_else(|| {
                AdapterError::upstream("Copilot exceeded the cumulative stream byte limit.")
            })?;
        let mut events = Vec::new();
        for &byte in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    self.count_byte()?;
                    self.end_line(&mut events)?;
                    continue;
                }
                self.end_line(&mut events)?;
            }
            self.count_byte()?;
            match byte {
                b'\r' => self.pending_cr = true,
                b'\n' => self.end_line(&mut events)?,
                _ => self.line.push(byte),
            }
        }
        Ok(events)
    }

    fn finish_inner(&mut self) -> Result<Vec<SseEvent>> {
        let mut events = Vec::new();
        if self.pending_cr {
            self.pending_cr = false;
            self.end_line(&mut events)?;
        } else if !self.line.is_empty() {
            self.end_line(&mut events)?;
        }
        self.dispatch(&mut events)?;
        Ok(events)
    }

    fn count_byte(&mut self) -> Result<()> {
        self.event_bytes = self
            .event_bytes
            .checked_add(1)
            .filter(|size| *size <= self.limits.max_event_bytes)
            .ok_or_else(|| {
                AdapterError::upstream("Copilot returned an oversized streaming event.")
            })?;
        Ok(())
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) -> Result<()> {
        let mut bytes = std::mem::take(&mut self.line);
        let result = match std::str::from_utf8(&bytes) {
            Ok(line) => self.consume_line(line, events),
            Err(_) => Err(AdapterError::upstream(
                "Copilot returned invalid streaming text.",
            )),
        };
        bytes.clear();
        self.line = bytes;
        result
    }

    fn consume_line(&mut self, line: &str, events: &mut Vec<SseEvent>) -> Result<()> {
        let line = if self.first_line {
            self.first_line = false;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            self.dispatch(events)?;
            self.event_bytes = 0;
        } else if let Some(comment) = line.strip_prefix(':') {
            events.push(SseEvent {
                comment: Some(comment.strip_prefix(' ').unwrap_or(comment).into()),
                ..SseEvent::default()
            });
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => {
                    self.event_type.clear();
                    self.event_type.push_str(value);
                }
                "data" => {
                    if self.has_data {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                    self.has_data = true;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) -> Result<()> {
        if self.has_data {
            let event = if self.data == "[DONE]" {
                SseEvent {
                    done: true,
                    ..SseEvent::default()
                }
            } else {
                let value = json::decode(self.data.as_bytes(), self.limits.max_event_bytes)
                    .map_err(|error| AdapterError::upstream(error.message))?;
                let object = value.as_object().ok_or_else(|| {
                    AdapterError::upstream("Copilot returned an invalid streaming event.")
                })?;
                let kind = match object.get("type") {
                    Some(value) => value.as_str().ok_or_else(|| {
                        AdapterError::upstream("Copilot returned an invalid event type.")
                    })?,
                    None => &self.event_type,
                };
                if invalid_kind(kind) {
                    return Err(AdapterError::upstream(
                        "Copilot returned an invalid event type.",
                    ));
                }
                SseEvent::json(kind.to_owned(), value)
            };
            events.push(event);
        }
        self.data.clear();
        self.event_type.clear();
        self.has_data = false;
        Ok(())
    }
}
