//! GitHub identity and OAuth device flow. No credential persistence or UI.

use crate::context::RequestContext;
use crate::transport::{self, Redaction};
use adapter_protocol::{AdapterError, Result, Value};
use reqwest::Client;
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, AUTHORIZATION};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;
use url::Url;

const API_ORIGIN: &str = "https://api.github.com";
const WEB_ORIGIN: &str = "https://github.com";
const COPILOT_ORIGIN: &str = "https://api.githubcopilot.com";

#[derive(Clone)]
pub struct Credential {
    pub login: String,
    token: Arc<str>,
}

impl Credential {
    pub fn new(login: String, token: String) -> Result<Self> {
        if login.trim().is_empty() || login.chars().any(char::is_control) {
            return Err(AdapterError::invalid(
                "A nonempty GitHub account login is required.",
            ));
        }
        let token = validate_token(&token, 401, "GitHub OAuth")?;
        Ok(Self {
            login,
            token: Arc::from(token),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("login", &Redaction::new(&[self.token()]).text(&self.login))
            .field("token", &"[redacted]")
            .finish()
    }
}

pub struct DeviceAuthorization {
    pub verification_uri: String,
    pub user_code: String,
    pub expires_in: Duration,
    device_code: String,
    client_id: String,
    interval: Duration,
    expires_at: Instant,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redaction = Redaction::new(&[&self.device_code]);
        formatter
            .debug_struct("DeviceAuthorization")
            .field("verification_uri", &redaction.text(&self.verification_uri))
            .field("user_code", &redaction.text(&self.user_code))
            .field("expires_in", &self.expires_in)
            .field("device_code", &"[redacted]")
            .finish()
    }
}

fn validate_token(token: &str, status: u16, label: &str) -> Result<String> {
    let token = token.trim();
    if token.is_empty() || !token.bytes().all(|byte| (33..=126).contains(&byte)) {
        return Err(AdapterError::new(
            status,
            if status == 401 {
                "invalid_credential"
            } else {
                "upstream_error"
            },
            format!("A valid nonempty {label} token is required."),
        ));
    }
    Ok(token.to_owned())
}

fn client_id(value: &str) -> Result<&str> {
    let value = value.trim();
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(AdapterError::invalid(
            "A GitHub OAuth client ID is required for device sign-in.",
        ));
    }
    Ok(value)
}

fn expected_account(expected: Option<&str>) -> Result<()> {
    if expected.is_some_and(|login| login.trim().is_empty() || login.chars().any(char::is_control))
    {
        return Err(AdapterError::invalid(
            "The expected GitHub account login must be nonempty.",
        ));
    }
    Ok(())
}

fn success(status: u16, operation: &str) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let message = match status {
        401 => "The selected GitHub credential was rejected. Sign in again.".to_owned(),
        403 => "The selected GitHub account cannot access Copilot. Check its Copilot subscription, organization policy, and OAuth authorization.".to_owned(),
        429 => "GitHub rate-limited the selected account. Try again later.".to_owned(),
        _ => format!("{operation} failed with HTTP {status}."),
    };
    Err(AdapterError::new(
        transport::error_status(status),
        "authentication_error",
        message,
    ))
}

#[derive(Clone)]
pub(crate) struct AuthClient {
    client: Client,
    api: Url,
    web: Url,
    #[cfg(test)]
    copilot_fixtures: Vec<Url>,
}

impl AuthClient {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: transport::client(false)?,
            api: Url::parse(API_ORIGIN).map_err(|_| {
                AdapterError::new(500, "auth_configuration", "Invalid fixed GitHub origin.")
            })?,
            web: Url::parse(WEB_ORIGIN).map_err(|_| {
                AdapterError::new(500, "auth_configuration", "Invalid fixed GitHub origin.")
            })?,
            #[cfg(test)]
            copilot_fixtures: Vec::new(),
        })
    }

    #[cfg(test)]
    pub(crate) fn fixture(origin: Url) -> Result<Self> {
        if !transport::loopback(&origin) {
            return Err(AdapterError::invalid(
                "Fixture origins must be loopback-only.",
            ));
        }
        Ok(Self {
            client: transport::client(true)?,
            api: origin.clone(),
            web: origin.clone(),
            copilot_fixtures: vec![origin],
        })
    }

    async fn json(
        &self,
        path: AuthPath,
        token: Option<&str>,
        form: Option<&[(&str, &str)]>,
        context: &RequestContext,
    ) -> Result<(u16, Value)> {
        let (base, suffix, post) = match path {
            AuthPath::User => (&self.api, "/user", false),
            AuthPath::CopilotToken => (&self.api, "/copilot_internal/v2/token", false),
            AuthPath::Device => (&self.web, "/login/device/code", true),
            AuthPath::Poll => (&self.web, "/login/oauth/access_token", true),
        };
        if post != form.is_some() {
            return Err(AdapterError::new(
                500,
                "auth_configuration",
                "Invalid authentication method.",
            ));
        }
        let url = transport::append_path(base, suffix)?;
        let mut request = if post {
            self.client.post(url)
        } else {
            self.client.get(url)
        };
        request = request
            .header(ACCEPT, "application/json")
            .header(ACCEPT_ENCODING, "identity")
            .timeout(transport::timeout(
                context,
                Some(transport::CONTROL_TIMEOUT),
            )?);
        if let Some(token) = token {
            let token = validate_token(token, 401, "GitHub OAuth")?;
            request = request.header(AUTHORIZATION, transport::bearer("token", &token)?);
        }
        if let Some(form) = form {
            request = request.form(form);
        }
        let response = transport::send(request, context, "GitHub authentication").await?;
        let status = response.status().as_u16();
        if matches!(status, 401 | 403) {
            context.check()?;
            success(status, "GitHub authentication")?;
        }
        let payload = transport::read_json(
            response,
            context,
            transport::AUTH_BYTES,
            "GitHub authentication",
        )
        .await?;
        Ok((status, payload))
    }

    pub(crate) async fn verify(
        &self,
        token: &str,
        expected: Option<&str>,
        context: &RequestContext,
    ) -> Result<Credential> {
        expected_account(expected)?;
        let token = validate_token(token, 401, "GitHub OAuth")?;
        let redaction = Redaction::new(&[&token]);
        let result = async {
            let (status, payload) = self.json(AuthPath::User, Some(&token), None, context).await?;
            success(status, "GitHub account lookup")?;
            let login = payload.get("login").and_then(Value::as_str)
                .filter(|login| !login.trim().is_empty() && !login.chars().any(char::is_control))
                .ok_or_else(|| AdapterError::upstream("GitHub did not return an account login."))?;
            if expected.is_some_and(|expected| !login.eq_ignore_ascii_case(expected)) {
                return Err(AdapterError::new(
                    403, "account_mismatch",
                    format!("The selected account is {login}, not {}. No account switch was performed; no fallback was attempted.", expected.unwrap_or_default()),
                ));
            }
            Credential::new(login.to_owned(), token.clone())
        }.await;
        result.map_err(|error| redaction.error(error))
    }

    async fn begin(&self, client: &str, context: &RequestContext) -> Result<DeviceAuthorization> {
        let client = client_id(client)?;
        let (status, payload) = self
            .json(
                AuthPath::Device,
                None,
                Some(&[("client_id", client), ("scope", "read:user")]),
                context,
            )
            .await?;
        success(status, "GitHub device sign-in")?;
        if payload
            .get("error")
            .is_some_and(|error| !error.is_null() && error != "")
        {
            return Err(AdapterError::invalid(
                "GitHub rejected device sign-in. Check that this OAuth client enables it.",
            ));
        }
        let verification_uri = payload
            .get("verification_uri")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AdapterError::upstream("GitHub returned an invalid verification URL.")
            })?;
        let verification = transport::endpoint(verification_uri)
            .map_err(|_| AdapterError::upstream("GitHub returned an invalid verification URL."))?;
        if verification.scheme() != "https"
            || verification.host_str() != Some("github.com")
            || verification.port_or_known_default() != Some(443)
            || verification.path() != "/login/device"
        {
            return Err(AdapterError::upstream(
                "GitHub returned an invalid verification URL.",
            ));
        }
        let user_code = payload
            .get("user_code")
            .and_then(Value::as_str)
            .filter(|code| {
                !code.trim().is_empty() && code.bytes().all(|byte| (32..=126).contains(&byte))
            })
            .ok_or_else(|| {
                AdapterError::upstream("GitHub returned invalid device sign-in details.")
            })?;
        let device_code = payload
            .get("device_code")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AdapterError::upstream("GitHub returned invalid device sign-in details.")
            })?;
        let device_code = validate_token(device_code, 502, "device authorization")?;
        let interval = positive_reply(payload.get("interval"), "device polling interval")?;
        let expires_in =
            Duration::from_secs(positive_reply(payload.get("expires_in"), "device expiry")?);
        let expires_at = Instant::now()
            .checked_add(expires_in)
            .ok_or_else(|| AdapterError::upstream("GitHub returned an invalid device expiry."))?;
        Ok(DeviceAuthorization {
            verification_uri: verification_uri.to_owned(),
            user_code: user_code.to_owned(),
            device_code,
            client_id: client.to_owned(),
            interval: Duration::from_secs(interval),
            expires_in,
            expires_at,
        })
    }

    async fn poll(
        &self,
        client: &str,
        authorization: DeviceAuthorization,
        expected: Option<&str>,
        context: &RequestContext,
    ) -> Result<Credential> {
        let client = client_id(client)?;
        expected_account(expected)?;
        if client != authorization.client_id {
            return Err(AdapterError::invalid(
                "Device authorization belongs to a different OAuth client.",
            ));
        }
        let redaction = Redaction::new(&[&authorization.device_code]);
        let result = async {
            let mut interval = authorization.interval;
            loop {
                context.check()?;
                let next = Instant::now().checked_add(interval);
                if next.is_none_or(|next| next >= authorization.expires_at) {
                    return Err(expired_device());
                }
                context.bounded(async {
                    tokio::time::sleep(interval).await;
                    Ok(())
                }).await?;
                if Instant::now() >= authorization.expires_at {
                    return Err(expired_device());
                }
                let poll_context = RequestContext {
                    cancellation: context.cancellation.clone(),
                    deadline: context.deadline.min(authorization.expires_at),
                };
                let reply = self.json(AuthPath::Poll, None, Some(&[
                    ("client_id", client),
                    ("device_code", authorization.device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]), &poll_context).await;
                if Instant::now() >= authorization.expires_at && !context.cancellation.is_cancelled() {
                    return Err(expired_device());
                }
                let (status, payload) = reply?;
                if status != 200 && status != 400 {
                    success(status, "GitHub device sign-in")?;
                }
                if payload.get("error").is_some_and(|error| !error.is_null() && !error.is_string()) {
                    return Err(AdapterError::upstream("GitHub returned an invalid device sign-in error."));
                }
                match payload.get("error").and_then(Value::as_str).filter(|error| !error.is_empty()) {
                    Some("authorization_pending") => continue,
                    Some("slow_down") => {
                        interval = slowed_interval(interval, payload.get("interval"))?;
                        continue;
                    }
                    Some("access_denied") => return Err(AdapterError::new(401, "access_denied", "GitHub sign-in was declined.")),
                    Some("expired_token") => return Err(expired_device()),
                    Some(_) => return Err(AdapterError::invalid("GitHub device sign-in failed. Restart login and check the OAuth client.")),
                    None => {}
                }
                success(status, "GitHub device sign-in")?;
                if payload.get("token_type").is_some_and(|kind| !kind.as_str().is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))) {
                    return Err(AdapterError::upstream("GitHub returned an invalid OAuth token type."));
                }
                let token = payload.get("access_token").and_then(Value::as_str)
                    .ok_or_else(|| AdapterError::upstream("GitHub returned an invalid OAuth token."))?;
                let token = validate_token(token, 502, "GitHub OAuth")?;
                return self.verify(&token, expected, context).await;
            }
        }.await;
        result.map_err(|error| redaction.error(error))
    }

    fn copilot_endpoint(&self, value: &str) -> Result<Url> {
        let endpoint = transport::endpoint(value).map_err(|_| {
            AdapterError::upstream(
                "GitHub returned an unexpected Copilot API endpoint; refusing it.",
            )
        })?;
        let trusted = endpoint.scheme() == "https"
            && endpoint.port_or_known_default() == Some(443)
            && endpoint.host_str().is_some_and(|host| {
                host == "api.githubcopilot.com" || host.ends_with(".githubcopilot.com")
            });
        #[cfg(test)]
        let trusted = if self.copilot_fixtures.is_empty() {
            trusted
        } else {
            self.copilot_fixtures.iter().any(|origin| {
                transport::loopback(&endpoint) && endpoint.origin() == origin.origin()
            })
        };
        if !trusted {
            return Err(AdapterError::upstream(
                "GitHub returned an unexpected Copilot API endpoint; refusing it.",
            ));
        }
        Ok(endpoint)
    }
}

enum AuthPath {
    User,
    Device,
    Poll,
    CopilotToken,
}

fn positive_reply(value: Option<&Value>, label: &str) -> Result<u64> {
    value
        .and_then(transport::unsigned)
        .filter(|value| *value > 0)
        .ok_or_else(|| AdapterError::upstream(format!("GitHub returned an invalid {label}.")))
}

fn expired_device() -> AdapterError {
    AdapterError::new(
        401,
        "expired_token",
        "The GitHub device code expired. Run login again.",
    )
}

fn slowed_interval(interval: Duration, requested: Option<&Value>) -> Result<Duration> {
    let minimum = interval
        .checked_add(Duration::from_secs(5))
        .ok_or_else(|| {
            AdapterError::upstream("GitHub returned an invalid device polling interval.")
        })?;
    Ok(requested
        .and_then(transport::unsigned)
        .filter(|interval| *interval > 0)
        .map(Duration::from_secs)
        .map_or(minimum, |interval| minimum.max(interval)))
}

pub async fn verify_account(
    token: &str,
    expected_login: Option<&str>,
    ctx: &RequestContext,
) -> Result<Credential> {
    validate_token(token, 401, "GitHub OAuth")?;
    expected_account(expected_login)?;
    ctx.check()?;
    AuthClient::new()?.verify(token, expected_login, ctx).await
}

pub async fn begin_device_login(
    client_id: &str,
    ctx: &RequestContext,
) -> Result<DeviceAuthorization> {
    self::client_id(client_id)?;
    ctx.check()?;
    AuthClient::new()?.begin(client_id, ctx).await
}

pub async fn poll_device_login(
    client_id: &str,
    authorization: DeviceAuthorization,
    expected_login: Option<&str>,
    ctx: &RequestContext,
) -> Result<Credential> {
    self::client_id(client_id)?;
    expected_account(expected_login)?;
    ctx.check()?;
    AuthClient::new()?
        .poll(client_id, authorization, expected_login, ctx)
        .await
}

pub(crate) struct CopilotSession {
    pub(crate) client: Client,
    pub(crate) endpoint: Url,
    token: Arc<str>,
    expires_at: Instant,
    refresh_at: Instant,
}

impl CopilotSession {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn check(&self) -> Result<()> {
        if Instant::now() >= self.expires_at {
            return Err(AdapterError::new(
                401,
                "authorization_expired",
                "The selected Copilot authorization expired before inference. No request was replayed.",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for CopilotSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CopilotSession")
            .field("token", &"[redacted]")
            .field("refresh_at", &self.refresh_at)
            .finish_non_exhaustive()
    }
}

pub(crate) struct CopilotAuth {
    pub(crate) credential: Credential,
    http: AuthClient,
    current: Mutex<Option<Arc<CopilotSession>>>,
    refresh: AsyncMutex<()>,
}

impl CopilotAuth {
    pub(crate) fn new(credential: Credential, http: AuthClient) -> Self {
        Self {
            credential,
            http,
            current: Mutex::new(None),
            refresh: AsyncMutex::new(()),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Arc<CopilotSession>>>> {
        self.current.lock().map_err(|_| {
            AdapterError::new(
                500,
                "authorization_unavailable",
                "The selected authorization state is unavailable.",
            )
        })
    }

    pub(crate) fn invalidate(&self, session: &Arc<CopilotSession>) -> Result<()> {
        let mut current = self.lock()?;
        if current
            .as_ref()
            .is_some_and(|value| Arc::ptr_eq(value, session))
        {
            *current = None;
        }
        Ok(())
    }

    pub(crate) async fn session(&self, context: &RequestContext) -> Result<Arc<CopilotSession>> {
        context.check()?;
        if let Some(session) = self
            .lock()?
            .as_ref()
            .filter(|session| Instant::now() < session.refresh_at)
        {
            return Ok(session.clone());
        }
        let _refresh = context
            .bounded(async { Ok(self.refresh.lock().await) })
            .await?;
        if let Some(session) = self
            .lock()?
            .as_ref()
            .filter(|session| Instant::now() < session.refresh_at)
        {
            return Ok(session.clone());
        }
        let previous = self.lock()?.take();
        let redaction = Redaction::new(&[self.credential.token()]);
        let result = async {
            let (status, payload) = self
                .http
                .json(
                    AuthPath::CopilotToken,
                    Some(self.credential.token()),
                    None,
                    context,
                )
                .await?;
            success(status, "Copilot authorization")?;
            let token = payload
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AdapterError::upstream("GitHub returned invalid Copilot authorization.")
                })?;
            let token = validate_token(token, 502, "Copilot service")?;
            let expires = positive_reply(payload.get("expires_at"), "Copilot expiry")?;
            let now_wall = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
                AdapterError::new(
                    500,
                    "clock_error",
                    "The local clock predates the Unix epoch.",
                )
            })?;
            let remaining = Duration::from_secs(expires)
                .checked_sub(now_wall)
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    AdapterError::upstream("GitHub returned expired Copilot authorization.")
                })?;
            let endpoint = match payload.get("endpoints") {
                None | Some(Value::Null) => self.http.copilot_endpoint(COPILOT_ORIGIN)?,
                Some(Value::Object(endpoints)) => {
                    let endpoint = match endpoints.get("api") {
                        None => COPILOT_ORIGIN,
                        Some(value) => value.as_str().ok_or_else(|| {
                            AdapterError::upstream("GitHub did not return a Copilot API endpoint.")
                        })?,
                    };
                    self.http.copilot_endpoint(endpoint)?
                }
                Some(_) => {
                    return Err(AdapterError::upstream(
                        "GitHub returned invalid Copilot endpoints.",
                    ));
                }
            };
            let mut refresh_in = remaining - (remaining / 10).min(Duration::from_secs(60));
            if let Some(interval) = payload
                .get("refresh_in")
                .and_then(transport::unsigned)
                .filter(|interval| *interval > 0)
            {
                refresh_in = refresh_in.min(Duration::from_secs(interval));
            }
            let now = Instant::now();
            let expires_at = now.checked_add(remaining).ok_or_else(|| {
                AdapterError::upstream("GitHub returned an invalid Copilot expiry.")
            })?;
            let refresh_at = now.checked_add(refresh_in).ok_or_else(|| {
                AdapterError::upstream("GitHub returned an invalid Copilot refresh interval.")
            })?;
            let client = if let Some(previous) = previous
                .as_ref()
                .filter(|previous| previous.endpoint == endpoint)
            {
                previous.client.clone()
            } else {
                transport::client(transport::loopback(&endpoint))?
            };
            let session = Arc::new(CopilotSession {
                client,
                endpoint,
                token: Arc::from(token),
                expires_at,
                refresh_at,
            });
            *self.lock()? = Some(session.clone());
            Ok(session)
        }
        .await;
        result.map_err(|error| redaction.error(error))
    }
}

#[cfg(test)]
mod tests;
