use std::fmt;
use std::io::{self, Write};

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::error::{AdapterError, Result};

/// Maximum nesting of JSON objects/arrays, including the root container.
pub const MAX_JSON_DEPTH: usize = 64;
/// Maximum bytes in a numeric literal, including its sign, fraction and exponent.
///
/// Numbers remain arbitrary-precision JSON numbers: a finite decimal such as
/// `1e400` is not rounded through `f64`. NaN and infinities are not JSON literals.
pub const MAX_NUMBER_BYTES: usize = 4096;

pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Value> {
    if bytes.len() > max_bytes {
        return Err(AdapterError::invalid("JSON exceeds the byte limit."));
    }
    let private_keys = check_tokens(bytes)?;
    let result = if private_keys {
        serde_json::from_slice::<&RawValue>(bytes).and_then(decode_raw)
    } else {
        serde_json::from_slice(bytes)
    };
    result.map_err(|_| AdapterError::invalid("The body must contain valid UTF-8 JSON."))
}

// Value's arbitrary-precision/raw-value visitors reserve private map keys.
// A real upstream object with one of those keys must remain an object.
fn decode_raw(raw: &RawValue) -> serde_json::Result<Value> {
    match raw.get().trim_start().as_bytes().first() {
        Some(b'{') => {
            let fields: RawObject<'_> = serde_json::from_str(raw.get())?;
            let mut result = serde_json::Map::new();
            for (key, value) in fields.0 {
                result.insert(key, decode_raw(value)?);
            }
            Ok(Value::Object(result))
        }
        Some(b'[') => {
            let items: Vec<&RawValue> = serde_json::from_str(raw.get())?;
            items
                .into_iter()
                .map(decode_raw)
                .collect::<serde_json::Result<Vec<_>>>()
                .map(Value::Array)
        }
        _ => serde_json::from_str(raw.get()),
    }
}

struct RawObject<'de>(Vec<(String, &'de RawValue)>);

impl<'de> Deserialize<'de> for RawObject<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ObjectVisitor;

        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = RawObject<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut fields = Vec::new();
                while let Some(entry) = map.next_entry::<String, &'de RawValue>()? {
                    fields.push(entry);
                }
                Ok(RawObject(fields))
            }
        }

        deserializer.deserialize_map(ObjectVisitor)
    }
}

fn check_tokens(bytes: &[u8]) -> Result<bool> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut position = 0usize;
    let mut string_start = 0usize;
    let mut private_keys = false;
    while position < bytes.len() {
        let byte = bytes[position];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                let next = bytes[position + 1..]
                    .iter()
                    .find(|byte| !matches!(**byte, b' ' | b'\t' | b'\r' | b'\n'));
                if next == Some(&b':') {
                    let key = &bytes[string_start..=position];
                    private_keys |= if key.contains(&b'\\') {
                        serde_json::from_slice::<String>(key)
                            .is_ok_and(|key| key.starts_with("$serde_json::private::"))
                    } else {
                        key.starts_with(b"\"$serde_json::private::")
                    };
                }
            }
            position += 1;
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string_start = position;
            }
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_JSON_DEPTH {
                    return Err(AdapterError::invalid("JSON exceeds the nesting limit."));
                }
            }
            b'}' | b']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| AdapterError::invalid("The body must contain valid JSON."))?;
            }
            b'-' | b'0'..=b'9' => {
                let start = position;
                while position < bytes.len()
                    && matches!(
                        bytes[position],
                        b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'
                    )
                {
                    position += 1;
                }
                if position - start > MAX_NUMBER_BYTES {
                    return Err(AdapterError::invalid(
                        "JSON exceeds the numeric literal limit.",
                    ));
                }
                continue;
            }
            _ => {}
        }
        position += 1;
    }
    Ok(private_keys)
}

pub(crate) fn validate_value(value: &Value) -> Result<()> {
    fn visit(value: &Value, depth: usize) -> Result<()> {
        match value {
            Value::Array(items) => {
                if depth >= MAX_JSON_DEPTH {
                    return Err(AdapterError::invalid("JSON exceeds the nesting limit."));
                }
                for item in items {
                    visit(item, depth + 1)?;
                }
            }
            Value::Object(fields) => {
                if depth >= MAX_JSON_DEPTH {
                    return Err(AdapterError::invalid("JSON exceeds the nesting limit."));
                }
                for value in fields.values() {
                    visit(value, depth + 1)?;
                }
            }
            Value::Number(number) if number.to_string().len() > MAX_NUMBER_BYTES => {
                return Err(AdapterError::invalid(
                    "JSON exceeds the numeric literal limit.",
                ));
            }
            _ => {}
        }
        Ok(())
    }
    visit(value, 0)
}

pub(crate) fn nonnegative_integer(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
}

pub(crate) struct ByteLimit<W> {
    inner: W,
    limit: usize,
    written: usize,
}

impl<W> ByteLimit<W> {
    pub(crate) fn new(inner: W, limit: usize) -> Self {
        Self {
            inner,
            limit,
            written: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.written
    }
}

impl<W: Write> Write for ByteLimit<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.written) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "The encoded body exceeds the byte limit.",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
