use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, AdapterError>;

impl AdapterError {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", message)
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(502, "upstream_error", message)
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AdapterError {}
