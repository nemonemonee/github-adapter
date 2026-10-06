use adapter_app::windows_credentials;

#[cfg(target_os = "macos")]
#[test]
#[ignore = "Uses unique fixture Keychain entries; run explicitly on an unlocked test-user Keychain."]
fn mac_keychain_updates_and_deletes_only_the_selected_account() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let service = format!(
        "GitHubAdapter.KeychainFixture.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for account in ["alpha", "beta"] {
                let _ = windows_credentials::delete(&self.0, account);
            }
        }
    }
    let _cleanup = Cleanup(service.clone());
    assert!(
        windows_credentials::read(&service, "alpha")
            .unwrap()
            .is_none()
    );
    assert!(!windows_credentials::delete(&service, "alpha").unwrap());
    windows_credentials::write(&service, "alpha", "fixture-雪-token").unwrap();
    windows_credentials::write(&service, "beta", "independent-fixture").unwrap();
    windows_credentials::write(&service, "alpha", "updated-fixture").unwrap();
    assert_eq!(
        windows_credentials::read(&service, "alpha")
            .unwrap()
            .as_deref(),
        Some("updated-fixture")
    );
    assert!(windows_credentials::delete(&service, "alpha").unwrap());
    assert!(!windows_credentials::delete(&service, "alpha").unwrap());
    assert_eq!(
        windows_credentials::read(&service, "beta")
            .unwrap()
            .as_deref(),
        Some("independent-fixture")
    );
}

#[test]
fn invalid_arguments_fail_before_any_native_credential_access() {
    let marker = "fixture-private-value";
    for (service, username) in [
        ("", "fixture"),
        ("fixture-private-value\0target", "fixture"),
        ("fixture", "fixture-private-value\0user"),
    ] {
        for error in [
            windows_credentials::read(service, username).unwrap_err(),
            windows_credentials::write(service, username, marker).unwrap_err(),
            windows_credentials::delete(service, username).unwrap_err(),
        ] {
            assert_eq!(error.status, 400);
            assert!(!error.message.contains(marker));
            assert!(!format!("{error:?}").contains(marker));
        }
    }
    assert_eq!(
        windows_credentials::write("fixture", "fixture", &"x".repeat(2_562))
            .unwrap_err()
            .status,
        400
    );
    assert_eq!(
        windows_credentials::read(&"x".repeat(32_768), "fixture")
            .unwrap_err()
            .status,
        400
    );
    assert_eq!(
        windows_credentials::delete("fixture", &"x".repeat(514))
            .unwrap_err()
            .status,
        400
    );
}

#[cfg(not(any(windows, target_os = "macos")))]
#[test]
fn other_platforms_report_unsupported_without_plaintext_fallback() {
    for error in [
        windows_credentials::read("fixture", "fixture").unwrap_err(),
        windows_credentials::write("fixture", "fixture", "fixture-value").unwrap_err(),
        windows_credentials::delete("fixture", "fixture").unwrap_err(),
    ] {
        assert_eq!(error.status, 501);
        assert_eq!(error.code, "unsupported_platform");
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use serde_json::{Value, json};
    use std::time::{SystemTime, UNIX_EPOCH};

    const PREFIX: &str = "GitHubAdapter.CredentialCompatibility.";
    const ALPHA: &str = "fixture-alpha";
    const BETA: &str = "fixture-beta";

    struct Fixture {
        service: String,
        cleaned: bool,
    }

    impl Fixture {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("System clock must be after the Unix epoch.")
                .as_nanos();
            Self {
                service: format!("{PREFIX}{}.{}", std::process::id(), unique),
                cleaned: false,
            }
        }

        fn record(&self, role: &str, revision: u8) -> Value {
            json!({
                "version": 1,
                "login": format!("fixture-{role}"),
                "token": format!("synthetic-{}-{role}-{revision}-\u{96ea}-\u{1f680}", self.service),
            })
        }

        fn assert_read(&self, username: &str, role: &str, revision: u8) {
            let raw = windows_credentials::read(&self.service, username)
                .expect("Native synthetic credential read failed.")
                .expect("Synthetic credential was missing.");
            let value: Value =
                serde_json::from_str(&raw).expect("Invalid synthetic credential JSON.");
            assert!(
                value == self.record(role, revision),
                "Credential contents did not match; values are deliberately omitted."
            );
        }

        fn cleanup(&mut self) -> Result<(), String> {
            let mut succeeded = true;
            for username in [ALPHA, BETA] {
                if let Err(error) = windows_credentials::delete(&self.service, username) {
                    eprintln!(
                        "Synthetic credential cleanup failed for service {}: {}",
                        self.service, error.code
                    );
                    succeeded = false;
                }
            }
            self.cleaned = succeeded;
            if succeeded {
                Ok(())
            } else {
                Err(format!(
                    "Synthetic credential entries may remain for service {}.",
                    self.service
                ))
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if !self.cleaned {
                let _ = self.cleanup();
            }
        }
    }

    #[test]
    #[ignore = "Creates only owned synthetic Windows credentials in an isolated GitHubAdapter.CredentialCompatibility.* namespace."]
    fn native_primary_compound_and_delete_semantics_are_compatible() {
        let mut fixture = Fixture::new();
        assert!(
            windows_credentials::read(&fixture.service, ALPHA)
                .unwrap()
                .is_none()
        );
        assert!(!windows_credentials::delete(&fixture.service, ALPHA).unwrap());

        windows_credentials::write(
            &fixture.service,
            ALPHA,
            &fixture.record("alpha", 1).to_string(),
        )
        .expect("Rust could not write the first synthetic account.");
        fixture.assert_read(ALPHA, "alpha", 1);

        windows_credentials::write(
            &fixture.service,
            BETA,
            &fixture.record("beta", 2).to_string(),
        )
        .expect("Rust could not write the synthetic account switch.");
        fixture.assert_read(BETA, "beta", 2);
        fixture.assert_read(ALPHA, "alpha", 1);

        windows_credentials::write(
            &fixture.service,
            ALPHA,
            &fixture.record("alpha", 3).to_string(),
        )
        .expect("Rust could not switch back to the first synthetic account.");
        fixture.assert_read(ALPHA, "alpha", 3);
        fixture.assert_read(BETA, "beta", 2);

        windows_credentials::write(
            &fixture.service,
            ALPHA,
            &fixture.record("alpha", 4).to_string(),
        )
        .expect("Rust could not update the synthetic account.");
        fixture.assert_read(ALPHA, "alpha", 4);
        fixture.assert_read(BETA, "beta", 2);

        assert!(windows_credentials::delete(&fixture.service, ALPHA).unwrap());
        assert!(!windows_credentials::delete(&fixture.service, ALPHA).unwrap());
        assert!(
            windows_credentials::read(&fixture.service, ALPHA)
                .unwrap()
                .is_none()
        );
        fixture.assert_read(BETA, "beta", 2);
        assert!(windows_credentials::delete(&fixture.service, BETA).unwrap());
        assert!(
            windows_credentials::read(&fixture.service, BETA)
                .unwrap()
                .is_none()
        );

        fixture
            .cleanup()
            .expect("Native synthetic credential cleanup failed.");
    }
}
