//! A dormant per-user LaunchAgent only while temporary routing is owned.
//! The pipe guardian handles ordinary crashes; RunAtLoad handles the next login.
use super::*;
use sha2::{Digest, Sha256};

pub(super) struct Registration {
    path: PathBuf,
    contents: String,
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

impl Registration {
    pub(super) fn new(executable: &Path, owner: &mode::Owner) -> Result<Self> {
        Self::for_directory(executable, &mode::mode_paths(&owner.paths()).backup_dir)
    }
    pub(super) fn for_directory(executable: &Path, mode_directory: &Path) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| error("An absolute HOME is required for login recovery."))?;
        let directory = home.join("Library/LaunchAgents");
        let identity = format!(
            "{:x}",
            Sha256::digest(mode_directory.as_os_str().as_encoded_bytes())
        );
        let label = format!(
            "io.github.nemonemonee.github-adapter.recover.{}",
            &identity[..24]
        );
        let program = executable
            .parent()
            .ok_or_else(|| error("The recovery executable has no directory."))?
            .join("github-adapter-host");
        if !program.is_file() {
            return Err(error("The paired GUI recovery executable is missing."));
        }
        let program = program
            .to_str()
            .ok_or_else(|| error("The recovery executable must have a Unicode path."))?;
        let mode_directory = mode_directory
            .to_str()
            .ok_or_else(|| error("The recovery directory must have a Unicode path."))?;
        let contents = format!(
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
                "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
                "<plist version=\"1.0\"><dict>\n",
                "<key>Label</key><string>{}</string>\n",
                "<key>ProgramArguments</key><array><string>{}</string><string>--logon-recovery</string><string>{}</string></array>\n",
                "<key>RunAtLoad</key><true/><key>LaunchOnlyOnce</key><true/>\n",
                "<key>KeepAlive</key><false/><key>ProcessType</key><string>Background</string>\n",
                "</dict></plist>\n"
            ),
            xml(&label),
            xml(program),
            xml(mode_directory)
        );
        Ok(Self {
            path: directory.join(format!("{label}.plist")),
            contents,
        })
    }
    pub(super) fn arm(&self) -> Result<()> {
        // Deliberately do not bootstrap: no login helper runs while the live owner holds routing.
        mode::update_login_registration(&self.path, self.contents.as_bytes(), true)
    }
    pub(super) fn disarm(&self) -> Result<()> {
        mode::update_login_registration(&self.path, self.contents.as_bytes(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owned_registration_is_exact_and_preserves_unrelated_files() {
        let root = tempfile::tempdir().unwrap();
        let registration = Registration {
            path: root.path().join("Library/LaunchAgents/test.plist"),
            contents: "<plist>owned &amp; quoted</plist>".into(),
        };
        registration.arm().unwrap();
        registration.arm().unwrap();
        assert_eq!(
            std::fs::read(&registration.path).unwrap(),
            registration.contents.as_bytes()
        );
        std::fs::write(&registration.path, b"unrelated").unwrap();
        assert!(registration.arm().is_err());
        registration.disarm().unwrap();
        assert_eq!(std::fs::read(&registration.path).unwrap(), b"unrelated");
        std::fs::write(&registration.path, registration.contents.as_bytes()).unwrap();
        registration.disarm().unwrap();
        assert!(!registration.path.exists());
        assert_eq!(xml("<&\"'>"), "&lt;&amp;&quot;&apos;&gt;");
    }
    #[test]
    fn pid_creation_distinguishes_current_owner_from_missing_process() {
        assert!(process_created(std::process::id()).unwrap().is_some());
        assert_eq!(process_created(i32::MAX as u32).unwrap(), None);
    }
}
