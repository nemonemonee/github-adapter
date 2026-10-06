//! Identity-verified desktop discovery and activation.

use adapter_protocol::{AdapterError, Result};
use std::collections::BTreeMap;

#[cfg(any(windows, test))]
const CODEX_NAME: &str = "OpenAI.Codex";
#[cfg(any(windows, test))]
const CHATGPT_NAME: &str = "OpenAI.ChatGPT-Desktop";
#[cfg(any(windows, test))]
const PUBLISHER_ID: &str = "2p2nqsd0c76g0";
const CODEX_FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";
const CHATGPT_FAMILY: &str = "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopApp {
    pub name: String,
    pub target: String,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    Codex,
    ChatGpt,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ChatGpt => "chatgpt",
        }
    }
}

fn invalid() -> AdapterError {
    AdapterError::invalid(
        "Choose codex or chatgpt with a supported official packaged-app identity; paths, URLs and executables are not accepted.",
    )
}

#[cfg(any(windows, target_os = "macos", test))]
fn failure(code: &str, message: &str) -> AdapterError {
    AdapterError::new(502, code, message)
}

fn missing() -> AdapterError {
    AdapterError::new(
        404,
        "desktop_not_found",
        "The selected desktop app is not available as a supported current-user installation. Nothing was installed or substituted.",
    )
}

fn ambiguous() -> AdapterError {
    AdapterError::new(
        409,
        "desktop_ambiguous",
        "Multiple distinct desktop identities or package registrations match. Resolve the ambiguity explicitly; no app was launched.",
    )
}

fn family_kind(family: &str) -> Option<Kind> {
    if family.eq_ignore_ascii_case(CODEX_FAMILY) {
        Some(Kind::Codex)
    } else if family.eq_ignore_ascii_case(CHATGPT_FAMILY) {
        Some(Kind::ChatGpt)
    } else {
        None
    }
}

fn target_kind(target: &str) -> Result<Kind> {
    match target {
        "com.openai.codex" => return Ok(Kind::Codex),
        "com.openai.chat" => return Ok(Kind::ChatGpt),
        _ => {}
    }
    if target.len() > 128 || !target.is_ascii() {
        return Err(invalid());
    }
    let (family, application) = target.split_once('!').ok_or_else(invalid)?;
    let kind = family_kind(family).ok_or_else(invalid)?;
    if application.is_empty()
        || application.len() > 64
        || !application.as_bytes()[0].is_ascii_alphabetic()
        || !application
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid());
    }
    Ok(kind)
}

fn validate(app: &DesktopApp) -> Result<Kind> {
    let kind = target_kind(&app.target)?;
    if app.name != kind.name() && !(kind == Kind::Codex && app.name == "chatgpt") {
        return Err(invalid());
    }
    Ok(kind)
}

pub fn select(name: &str, apps: &[DesktopApp]) -> Result<DesktopApp> {
    if !matches!(name, "codex" | "chatgpt") {
        return Err(invalid());
    }
    let checked = apps
        .iter()
        .map(|app| validate(app).map(|kind| (kind, app)))
        .collect::<Result<Vec<_>>>()?;
    let genuine_chatgpt = checked.iter().any(|(kind, _)| *kind == Kind::ChatGpt);
    let mut matches = BTreeMap::new();
    for (kind, app) in checked {
        let accepted = match name {
            "codex" => kind == Kind::Codex,
            _ if genuine_chatgpt => kind == Kind::ChatGpt,
            _ => kind == Kind::Codex && app.name == "chatgpt",
        };
        if accepted {
            matches
                .entry(app.target.to_ascii_lowercase())
                .or_insert_with(|| DesktopApp {
                    name: name.to_owned(),
                    target: app.target.clone(),
                });
        }
    }
    match matches.len() {
        0 => Err(missing()),
        1 => Ok(matches
            .into_values()
            .next()
            .expect("one validated identity")),
        _ => Err(ambiguous()),
    }
}

pub fn discover() -> Result<Vec<DesktopApp>> {
    #[cfg(windows)]
    {
        native::on_mta(|| {
            let mut catalog = native::Catalog::new()?;
            engine::discover(&mut catalog)
                .map(|entries| entries.into_iter().map(|entry| entry.app).collect())
        })
    }
    #[cfg(target_os = "macos")]
    {
        macos::discover()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(unsupported())
    }
}

pub fn launch(app: &DesktopApp) -> Result<()> {
    validate(app)?;
    #[cfg(windows)]
    {
        native::on_mta(|| engine::launch(app, &mut native::Catalog::new()?))
    }
    #[cfg(target_os = "macos")]
    {
        macos::launch(app)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(unsupported())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn unsupported() -> AdapterError {
    AdapterError::new(
        501,
        "unsupported_platform",
        "Native desktop discovery and activation require Windows or macOS.",
    )
}

#[cfg(any(windows, test))]
mod engine;

#[cfg(any(windows, test))]
mod apartment;

#[cfg(any(windows, test))]
fn wait_for_completion(
    mut status: impl FnMut() -> Result<i32>,
    mut cancel: impl FnMut(),
    budget: std::time::Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match status()? {
            1..=3 => return Ok(()),
            0 => {}
            _ => {
                return Err(failure(
                    "desktop_catalog_invalid",
                    "Windows returned an invalid async operation state.",
                ));
            }
        }
        if std::time::Instant::now() >= deadline {
            cancel();
            return Err(AdapterError::new(
                504,
                "desktop_timeout",
                "The Windows desktop operation did not acknowledge completion in time. Cancellation was requested; activation may still complete. No retry was issued.",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(windows)]
mod native;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(test)]
mod tests;
