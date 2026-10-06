//! Launch exact signed app bundles, never arbitrary URLs or executable commands.
use super::*;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const APPS: &[(&str, &str, &str)] = &[
    ("codex", "com.openai.codex", "Codex.app"),
    ("chatgpt", "com.openai.chat", "ChatGPT.app"),
];
// OpenAI's published app association identifies its legacy consumer app signing team.
// The current Codex bundle's signer must also be confirmed on a receiving Mac.
// https://openai.com/.well-known/apple-app-site-association
const OPENAI_TEAM: &str = "2DC432GLL2";

fn operation(program: &str, args: &[&OsStr]) -> Result<(bool, Vec<u8>)> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            failure(
                "desktop_catalog_unavailable",
                "The macOS desktop operation could not start.",
            )
        })?;
    let output = child.stdout.take().ok_or_else(|| {
        failure(
            "desktop_catalog_unavailable",
            "The desktop response is unavailable.",
        )
    })?;
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output.take(1024 * 1024 + 1).read_to_end(&mut bytes);
        let _ = send.send(result.map(|_| bytes));
    });
    let deadline = Instant::now() + Duration::from_secs(4);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(failure(
                    "desktop_timeout",
                    "The macOS desktop operation did not acknowledge completion in time. No activation was retried.",
                ));
            }
        }
    };
    let bytes = receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| failure("desktop_timeout", "The desktop response did not complete."))?
        .map_err(|_| {
            failure(
                "desktop_catalog_invalid",
                "The desktop response could not be read.",
            )
        })?;
    if bytes.len() > 1024 * 1024 {
        return Err(failure(
            "desktop_catalog_invalid",
            "The desktop response exceeded its size limit.",
        ));
    }
    Ok((status.success(), bytes))
}

fn verified(path: &Path, bundle_id: &str) -> Result<bool> {
    if !path.is_dir() || path.extension() != Some(OsStr::new("app")) {
        return Ok(false);
    }
    let plist = path.join("Contents/Info.plist");
    let (ok, identity) = operation(
        "/usr/bin/plutil",
        &[
            OsStr::new("-extract"),
            OsStr::new("CFBundleIdentifier"),
            OsStr::new("raw"),
            OsStr::new("-o"),
            OsStr::new("-"),
            plist.as_os_str(),
        ],
    )?;
    if !ok || String::from_utf8_lossy(&identity).trim() != bundle_id {
        return Ok(false);
    }
    let requirement = format!(
        "anchor apple generic and identifier \"{bundle_id}\" and certificate leaf[subject.OU] = \"{OPENAI_TEAM}\""
    );
    signature_matches(path, &requirement)
}

fn signature_matches(path: &Path, expression: &str) -> Result<bool> {
    // Without '=', codesign interprets -R's value as a requirements file path.
    let requirement = format!("={expression}");
    let (valid, _) = operation(
        "/usr/bin/codesign",
        &[
            OsStr::new("--verify"),
            OsStr::new("--strict"),
            OsStr::new("--deep"),
            OsStr::new("-R"),
            OsStr::new(&requirement),
            path.as_os_str(),
        ],
    )?;
    Ok(valid)
}

fn paths(bundle_id: &str, filename: &str) -> Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    let mut candidates = vec![PathBuf::from("/Applications").join(filename)];
    if bundle_id == "com.openai.codex" {
        candidates.push(PathBuf::from("/Applications/ChatGPT.app"));
    }
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        candidates.push(home.join("Applications").join(filename));
        if bundle_id == "com.openai.codex" {
            candidates.push(home.join("Applications/ChatGPT.app"));
        }
    }
    // Bundle-ID lookup also finds the unified app whose display name is ChatGPT.
    let query = format!("kMDItemCFBundleIdentifier == '{bundle_id}'");
    if let Ok((true, matches)) =
        operation("/usr/bin/mdfind", &[OsStr::new("-0"), OsStr::new(&query)])
    {
        for bytes in matches
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .take(128)
        {
            candidates.push(PathBuf::from(OsStr::from_bytes(bytes)));
        }
    }
    let mut matches = std::collections::BTreeSet::new();
    for candidate in candidates {
        if let Ok(path) = std::fs::canonicalize(candidate)
            && verified(&path, bundle_id)?
        {
            matches.insert(path);
        }
    }
    Ok(matches.into_iter().collect())
}
pub(super) fn discover() -> Result<Vec<DesktopApp>> {
    let mut apps = Vec::new();
    for &(name, bundle_id, filename) in APPS {
        let installed = paths(bundle_id, filename)?;
        match installed.as_slice() {
            [] => {}
            [path] => {
                apps.push(DesktopApp {
                    name: name.into(),
                    target: bundle_id.into(),
                });
                if bundle_id == "com.openai.codex"
                    && path.file_name() == Some(OsStr::new("ChatGPT.app"))
                {
                    apps.push(DesktopApp {
                        name: "chatgpt".into(),
                        target: bundle_id.into(),
                    });
                }
            }
            _ => return Err(ambiguous()),
        }
    }
    Ok(apps)
}
pub(super) fn launch(app: &DesktopApp) -> Result<()> {
    let &(_, bundle_id, filename) = APPS
        .iter()
        .find(|(_, id, _)| *id == app.target)
        .ok_or_else(invalid)?;
    let installed = paths(bundle_id, filename)?;
    let path = match installed.as_slice() {
        [] => return Err(missing()),
        [path] => path,
        _ => return Err(ambiguous()),
    };
    let (ok, _) = operation("/usr/bin/open", &[OsStr::new("-a"), path.as_os_str()])?;
    if ok {
        Ok(())
    } else {
        Err(failure(
            "desktop_launch_failed",
            "macOS did not acknowledge activation of the verified app.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_system_code_accepts_inline_requirement() {
        assert!(
            signature_matches(
                Path::new("/usr/bin/true"),
                "anchor apple and identifier \"com.apple.true\"",
            )
            .unwrap()
        );
    }

    #[test]
    fn signed_system_code_rejects_wrong_identifier() {
        assert!(
            !signature_matches(
                Path::new("/usr/bin/true"),
                "anchor apple and identifier \"com.openai.codex\"",
            )
            .unwrap()
        );
    }

    #[test]
    fn signed_system_code_rejects_wrong_signer() {
        let requirement = format!(
            "anchor apple and identifier \"com.apple.true\" and certificate leaf[subject.OU] = \"{OPENAI_TEAM}\""
        );
        assert!(!signature_matches(Path::new("/usr/bin/true"), &requirement).unwrap());
    }

    #[test]
    fn unsigned_same_id_bundle_is_not_an_official_app() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("Codex.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(bundle.join("Contents/Info.plist"), b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>com.openai.codex</string></dict></plist>").unwrap();
        assert!(!verified(&bundle, "com.openai.codex").unwrap());
    }

    #[test]
    #[ignore = "Read-only receiving-Mac check; set ADAPTER_TEST_CODEX_BUNDLE to the installed official app."]
    fn installed_codex_bundle_passes_verification_and_discovery() {
        let path = PathBuf::from(
            std::env::var_os("ADAPTER_TEST_CODEX_BUNDLE")
                .expect("Set ADAPTER_TEST_CODEX_BUNDLE to the installed official Codex app."),
        );
        assert!(path.is_absolute(), "Use the absolute installed app path.");
        let path = std::fs::canonicalize(path).unwrap();
        assert!(verified(&path, "com.openai.codex").unwrap());
        assert!(!verified(&path, "com.openai.chat").unwrap());
        assert!(
            !signature_matches(
                &path,
                "anchor apple generic and identifier \"com.openai.codex\" and certificate leaf[subject.OU] = \"0000000000\"",
            )
            .unwrap()
        );
        let apps = discover().unwrap();
        assert!(apps.contains(&DesktopApp {
            name: "codex".into(),
            target: "com.openai.codex".into(),
        }));
        if path.file_name() == Some(OsStr::new("ChatGPT.app")) {
            assert!(apps.contains(&DesktopApp {
                name: "chatgpt".into(),
                target: "com.openai.codex".into(),
            }));
        }
    }
}
