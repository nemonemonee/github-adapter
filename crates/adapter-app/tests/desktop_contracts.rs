use adapter_app::desktop::{DesktopApp, launch, select};

const CODEX: &str = "OpenAI.Codex_2p2nqsd0c76g0!App";
const CHATGPT: &str = "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0!App";

fn app(name: &str, target: &str) -> DesktopApp {
    DesktopApp {
        name: name.into(),
        target: target.into(),
    }
}

#[test]
fn macos_official_bundle_ids_preserve_codex_and_chatgpt_selection() {
    let available = [
        app("codex", "com.openai.codex"),
        app("chatgpt", "com.openai.chat"),
    ];
    assert_eq!(
        select("codex", &available).unwrap().target,
        "com.openai.codex"
    );
    assert_eq!(
        select("chatgpt", &available).unwrap().target,
        "com.openai.chat"
    );
    assert!(select("codex", &[app("codex", "com.openai.codex.untrusted")]).is_err());
    assert!(select("codex", &[app("codex", "COM.OPENAI.CODEX")]).is_err());
}

#[test]
fn desktop_select_recognizes_observed_codex_under_its_chatgpt_alias() {
    let alias = app("chatgpt", CODEX);
    assert_eq!(
        select("codex", std::slice::from_ref(&alias)).unwrap(),
        app("codex", CODEX)
    );
    assert_eq!(
        select("chatgpt", std::slice::from_ref(&alias)).unwrap(),
        alias
    );
}

#[test]
fn desktop_explicit_genuine_chatgpt_is_preferred_and_never_substituted_for_codex() {
    let available = [
        app("chatgpt", CODEX),
        app("codex", CODEX),
        app("chatgpt", CHATGPT),
    ];
    assert_eq!(select("codex", &available).unwrap().target, CODEX);
    assert_eq!(select("chatgpt", &available).unwrap().target, CHATGPT);
    assert_eq!(
        select("codex", &[app("chatgpt", CHATGPT)])
            .unwrap_err()
            .code,
        "desktop_not_found"
    );
    assert_eq!(
        select("chatgpt", &[app("codex", CODEX)]).unwrap_err().code,
        "desktop_not_found"
    );
}

#[test]
fn desktop_duplicates_of_one_identity_collapse_but_distinct_entries_are_ambiguous() {
    let available = [
        app("codex", CODEX),
        app("chatgpt", CODEX),
        app("codex", &CODEX.to_ascii_lowercase()),
    ];
    assert_eq!(select("codex", &available).unwrap().target, CODEX);
    let other = "OpenAI.Codex_2p2nqsd0c76g0!SecondApp";
    assert_eq!(
        select("codex", &[app("codex", CODEX), app("codex", other)])
            .unwrap_err()
            .code,
        "desktop_ambiguous"
    );
    assert_eq!(select("codex", &[]).unwrap_err().code, "desktop_not_found");
}

#[test]
fn desktop_invalid_inputs_are_rejected_before_any_native_launch() {
    for candidate in [
        app("unknown", CODEX),
        app("Codex", CODEX),
        app("codex", CHATGPT),
        app("codex", "cmd.exe"),
        app("codex", r"C:\fixture-private\app.exe"),
        app("codex", r"shell:AppsFolder\untrusted!App"),
        app("codex", "https://fixture-private.invalid"),
        app("codex", "OpenAI.Codex_untrusted!App"),
        app("codex", "OpenAI.Codex_1.0.0.0_arm64__2p2nqsd0c76g0!App"),
        app("codex", "OpenAI.Codex_2p2nqsd0c76g0!"),
        app("codex", "OpenAI.Codex_2p2nqsd0c76g0!App!Other"),
        app("codex", "OpenAI.Codex_2p2nqsd0c76g0!../App"),
        app("codex", "OpenAI.Codex_2p2nqsd0c76g0!App\n"),
        app("codex", "OpenAI.Codex_2p2nqsd0c76g0!应用"),
        app(
            "codex",
            &format!("OpenAI.Codex_2p2nqsd0c76g0!{}", "A".repeat(65)),
        ),
    ] {
        let error = launch(&candidate).unwrap_err();
        assert_eq!(error.status, 400);
        assert!(!error.message.contains("fixture-private"));
        assert!(select("codex", &[candidate]).is_err());
    }
    for requested in ["", "Codex", "chatgpt.exe", "codex --help"] {
        assert_eq!(
            select(requested, &[app("codex", CODEX)])
                .unwrap_err()
                .status,
            400
        );
    }
}

#[test]
fn desktop_untrusted_extra_catalog_entries_are_not_silently_accepted() {
    assert!(
        select(
            "codex",
            &[
                app("codex", CODEX),
                app("chatgpt", "https://fixture.invalid")
            ]
        )
        .is_err()
    );
}

#[cfg(windows)]
#[test]
fn desktop_implementation_has_no_shell_cli_or_installation_fallback() {
    let source = [
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "\\src\\desktop.rs")),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "\\src\\desktop\\engine.rs"
        )),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "\\src\\desktop\\apartment.rs"
        )),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "\\src\\desktop\\native.rs"
        )),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "\\src\\desktop\\tests.rs"
        )),
    ]
    .join("\n");
    for forbidden in [
        "std::process::Command",
        "Command::new(",
        "ShellExecute",
        "Get-StartApps",
        "AddPackageAsync",
        "RegisterPackageAsync",
        "RemovePackageAsync",
    ] {
        assert!(!source.contains(forbidden));
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
#[test]
fn desktop_non_windows_operations_report_unsupported_without_substituting_a_cli() {
    assert_eq!(adapter_app::desktop::discover().unwrap_err().status, 501);
    assert_eq!(launch(&app("codex", CODEX)).unwrap_err().status, 501);
}

#[cfg(windows)]
#[test]
#[ignore = "Read-only installed metadata; requires GITHUB_ADAPTER_DESKTOP_DISCOVERY=1. Never activates apps."]
fn desktop_native_discovery_read_only() {
    assert!(
        std::env::var("GITHUB_ADAPTER_DESKTOP_DISCOVERY")
            .ok()
            .as_deref()
            == Some("1"),
        "Explicitly enable read-only installed-package discovery."
    );
    let apps =
        adapter_app::desktop::discover().expect("Native read-only desktop discovery failed.");
    for candidate in &apps {
        assert!(select(&candidate.name, std::slice::from_ref(candidate)).is_ok());
    }
    if std::env::var("GITHUB_ADAPTER_DESKTOP_REQUIRE_CODEX")
        .ok()
        .as_deref()
        == Some("1")
    {
        assert!(
            select("codex", &apps).is_ok(),
            "Expected the observed Codex package to be discoverable."
        );
    }
    let from_sta = std::thread::spawn(|| {
        use windows::Win32::System::WinRT::{RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize};
        struct Sta;
        impl Drop for Sta {
            fn drop(&mut self) {
                unsafe { RoUninitialize() };
            }
        }
        unsafe { RoInitialize(RO_INIT_SINGLETHREADED) }
            .expect("Read-only STA fixture initialization failed.");
        let _sta = Sta;
        adapter_app::desktop::discover()
    })
    .join()
    .expect("Read-only STA discovery worker failed.")
    .expect("Read-only discovery from a caller-owned STA failed.");
    for candidate in &from_sta {
        assert!(select(&candidate.name, std::slice::from_ref(candidate)).is_ok());
    }
    eprintln!(
        "Read-only verification: {} normal and {} STA-origin capability records; no activation requested.",
        apps.len(),
        from_sta.len()
    );
}
