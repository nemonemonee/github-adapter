use adapter_protocol::{AdapterError, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_MESSAGE_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Starting,
    Ready,
    Failed,
    Stopping,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    NotRequested,
    Pending,
    Opening,
    Acknowledged,
    Failed,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Fault {
    pub code: String,
    pub message: String,
}

impl Fault {
    pub(crate) fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub(crate) fn from_error(error: &AdapterError, activation: bool) -> Self {
        let (code, message) = match error.code.as_str() {
            "missing_credential" => (
                "missing_credential",
                "No adapter GitHub credential is saved. Run login explicitly in a terminal, then stop/start the host.",
            ),
            "account_mismatch" => (
                "account_mismatch",
                "The saved GitHub account does not match. Stop the host before changing accounts.",
            ),
            "credential_conflict" => (
                "credential_conflict",
                "Adapter token environment overrides conflict. Correct them, then stop/start.",
            ),
            "restart_required" => (
                "restart_required",
                "Credentials changed. Stop and start the host; no replacement account or credential was selected.",
            ),
            "desktop_not_found" => (
                "desktop_not_found",
                "A supported current-user Codex installation was not found. Nothing was installed or substituted.",
            ),
            "desktop_ambiguous" => (
                "desktop_ambiguous",
                "Codex package identity is ambiguous. Nothing was launched.",
            ),
            "desktop_timeout" | "activation_timeout" if activation => (
                "activation_uncertain",
                "Windows did not acknowledge activation before the deadline. It may complete late. No automatic retry will occur.",
            ),
            "listener_unavailable" | "listener_in_use" | "address_in_use" | "bind_error" => (
                "listener_unavailable",
                "The requested listener is unavailable. No existing listener was reused or stopped.",
            ),
            "configuration_error" => (
                "configuration_error",
                "The selected client configuration is invalid. Inspect it with the explicit foreground doctor command before restarting.",
            ),
            "configuration_conflict" => (
                "configuration_conflict",
                "Client settings or recovery files conflict with their recorded ownership. Preserve your edits and inspect recovery before restarting.",
            ),
            "configuration_io_error" | "configuration_task_failed" => (
                "configuration_unavailable",
                "Client configuration could not be read or updated safely. Check file access and recovery with the explicit foreground doctor command.",
            ),
            "on_demand_recovery" => (
                "on_demand_recovery",
                "Temporary Codex routing could not be activated or restored safely. Preserve the recovery files and inspect normal-routing recovery before restarting.",
            ),
            "no_preferred_model" => (
                "no_preferred_model",
                "No preferred model is available in the selected account. Inspect its model list and select an available model explicitly.",
            ),
            "startup_deadline" => (
                "startup_deadline",
                "Backend startup exceeded its deadline. The owned operation was cancelled. Stop/start after resolving the cause.",
            ),
            "shutdown_deadline" => (
                "shutdown_deadline",
                "The owned server exceeded its drain deadline; remaining owned tasks were cancelled.",
            ),
            _ if activation => (
                "activation_failed",
                "Codex activation failed. The adapter remains available. Check the installed app with the foreground apps command.",
            ),
            _ => (
                "backend_failed",
                "Backend startup or serving failed. No fallback process was used. Run the equivalent explicit foreground command for detailed diagnostics, then stop/start.",
            ),
        };
        Self::new(code, message)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub state: State,
    pub address: Option<std::net::SocketAddr>,
    pub activation: Activation,
    pub activation_attempt: u64,
    pub diagnostics: Vec<Fault>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Start {
        specification: String,
        credentials: String,
    },
    Status,
    Open {
        credentials: String,
    },
    Stop,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u16,
    pub command: Control,
}

impl Request {
    pub fn new(command: Control) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            command,
        }
    }

    pub fn validate(&self) -> Result<()> {
        version(self.version)?;
        let valid =
            |text: &str| text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit());
        match &self.command {
            Control::Start {
                specification,
                credentials,
            } if !valid(specification) || !valid(credentials) => Err(invalid_schema()),
            Control::Open { credentials } if !valid(credentials) => Err(invalid_schema()),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u16,
    pub process_id: u32,
    pub snapshot: Snapshot,
    pub error: Option<Fault>,
    pub exited: bool,
}

pub(crate) fn invalid_schema() -> AdapterError {
    AdapterError::new(
        400,
        "host_protocol_invalid",
        "The control message does not match the bounded host protocol.",
    )
}

fn version(value: u16) -> Result<()> {
    if value != PROTOCOL_VERSION {
        Err(AdapterError::new(
            409,
            "host_protocol_version",
            "The host control protocol version is incompatible. Use the owning binary to stop it.",
        ))
    } else {
        Ok(())
    }
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE_BYTES {
        return Err(invalid_schema());
    }
    serde_json::from_slice(bytes).map_err(|_| invalid_schema())
}

pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    let request: Request = decode(bytes)?;
    request.validate()?;
    // Serde's internally tagged unit variants otherwise ignore surplus fields.
    let shape: serde_json::Value = decode(bytes)?;
    let expected = match request.command {
        Control::Status | Control::Stop => 1,
        Control::Open { .. } => 2,
        Control::Start { .. } => 3,
    };
    if shape
        .get("command")
        .and_then(serde_json::Value::as_object)
        .map(|command| command.len())
        != Some(expected)
    {
        return Err(invalid_schema());
    }
    Ok(request)
}

pub fn decode_response(bytes: &[u8]) -> Result<Response> {
    let response: Response = decode(bytes)?;
    version(response.version)?;
    if response.process_id == 0
        || response.snapshot.diagnostics.len() > 8
        || response
            .snapshot
            .diagnostics
            .iter()
            .chain(response.error.iter())
            .any(|fault| {
                fault.code.len() > 64
                    || fault.message.len() > 512
                    || fault.code.chars().any(char::is_control)
                    || fault.message.chars().any(char::is_control)
            })
        || response
            .snapshot
            .address
            .is_some_and(|address| !address.ip().is_loopback())
    {
        return Err(invalid_schema());
    }
    Ok(response)
}

pub fn encode_message(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid_schema())?;
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE_BYTES {
        return Err(invalid_schema());
    }
    Ok(bytes)
}

pub async fn read_message(stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let length = stream.read_u32_le().await.map_err(|_| io_failure())? as usize;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        return Err(invalid_schema());
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|_| io_failure())?;
    Ok(bytes)
}

pub async fn write_message(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> Result<()> {
    let bytes = encode_message(value)?;
    stream
        .write_u32_le(bytes.len() as u32)
        .await
        .map_err(|_| io_failure())?;
    stream.write_all(&bytes).await.map_err(|_| io_failure())?;
    stream.flush().await.map_err(|_| io_failure())
}

fn io_failure() -> AdapterError {
    AdapterError::new(
        503,
        "host_control_closed",
        "The authenticated host control connection closed before completing the request.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_listener_failures_keep_their_safe_category() {
        let listener = crate::listener::bind_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
        let error = crate::listener::bind_loopback(listener.local_addr().unwrap()).unwrap_err();
        assert_eq!(error.code, "listener_unavailable");
        assert_eq!(
            Fault::from_error(&error, false).code,
            "listener_unavailable"
        );
    }

    #[test]
    fn operational_categories_never_project_raw_details() {
        for (input, expected) in [
            ("listener_unavailable", "listener_unavailable"),
            ("listener_in_use", "listener_unavailable"),
            ("address_in_use", "listener_unavailable"),
            ("bind_error", "listener_unavailable"),
            ("configuration_error", "configuration_error"),
            ("configuration_conflict", "configuration_conflict"),
            ("configuration_io_error", "configuration_unavailable"),
            ("configuration_task_failed", "configuration_unavailable"),
            ("on_demand_recovery", "on_demand_recovery"),
            ("no_preferred_model", "no_preferred_model"),
            ("missing_credential", "missing_credential"),
            ("unknown_failure", "backend_failed"),
        ] {
            let fault = Fault::from_error(
                &AdapterError::new(
                    503,
                    input,
                    "PRIVATE-MARKER /secret/path token=synthetic-secret\n",
                ),
                false,
            );
            assert_eq!(fault.code, expected, "{input}");
            assert!(fault.message.len() <= 512);
            assert!(!fault.message.chars().any(char::is_control));
            assert!(
                !serde_json::to_string(&fault)
                    .unwrap()
                    .contains("PRIVATE-MARKER")
            );
            assert!(!fault.message.contains("synthetic-secret"));
        }
        assert_eq!(
            Fault::from_error(
                &AdapterError::new(503, "unknown_failure", "PRIVATE-MARKER"),
                true
            )
            .code,
            "activation_failed"
        );
    }
}
