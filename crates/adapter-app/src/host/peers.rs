use std::path::{Path, PathBuf};

use adapter_protocol::Result;

use super::failure;

const CLI_IMAGE: &str = "github-adapter.exe";
const DESKTOP_IMAGE: &str = "github-adapter-host.exe";

/// Local verification input, obtained from Windows process/token APIs, never IPC fields.
#[derive(Clone, Debug)]
pub struct PeerIdentity {
    pub image: PathBuf,
    pub user: String,
    pub session: u32,
}

/// Exact canonical image allowlist. Non-release executables are exact-self only.
#[derive(Clone)]
pub struct PeerPolicy {
    user: String,
    session: u32,
    canonical: Vec<PathBuf>,
    lookup: Vec<PathBuf>,
}

impl PeerPolicy {
    pub fn new(current: PeerIdentity) -> Result<Self> {
        let absolute = std::path::absolute(&current.image).map_err(|_| unavailable())?;
        let image = std::fs::canonicalize(&absolute).map_err(|_| unavailable())?;
        let counterpart = match image.file_name().and_then(|name| name.to_str()) {
            Some(CLI_IMAGE) => Some(DESKTOP_IMAGE),
            Some(DESKTOP_IMAGE) => Some(CLI_IMAGE),
            _ => None,
        };
        let mut canonical = vec![image.clone()];
        let mut lookup = vec![absolute.clone(), image.clone()];
        if let Some(counterpart) = counterpart {
            let directory = image.parent().ok_or_else(unavailable)?;
            let sibling = directory.join(counterpart);
            // Do not canonicalize a sibling symlink into a new trust anchor.
            canonical.push(sibling.clone());
            lookup.push(sibling);
            lookup.push(absolute.parent().ok_or_else(unavailable)?.join(counterpart));
        }
        Ok(Self {
            user: current.user,
            session: current.session,
            canonical,
            lookup,
        })
    }

    pub fn allows(&self, peer: &PeerIdentity) -> bool {
        if peer.user != self.user || peer.session != self.session {
            return false;
        }
        let Ok(absolute) = std::path::absolute(&peer.image) else {
            return false;
        };
        // Reject unrelated paths before touching their filesystem (including remote images).
        if !self
            .lookup
            .iter()
            .any(|allowed| lookup_matches(allowed, &absolute))
        {
            return false;
        }
        let Ok(canonical) = std::fs::canonicalize(&absolute) else {
            return false;
        };
        self.canonical.contains(&canonical)
    }
}

fn lookup_matches(allowed: &Path, candidate: &Path) -> bool {
    match (allowed.to_str(), candidate.to_str()) {
        (Some(allowed), Some(candidate)) => {
            let strip = |value: &str| match value.strip_prefix(r"\\?\UNC\") {
                Some(tail) => format!(r"\\{tail}"),
                None => value.strip_prefix(r"\\?\").unwrap_or(value).to_owned(),
            };
            strip(allowed).eq_ignore_ascii_case(&strip(candidate))
        }
        _ => false,
    }
}

fn unavailable() -> adapter_protocol::AdapterError {
    failure(
        "host_identity_unavailable",
        "Cannot establish the exact native executable paths.",
    )
}
