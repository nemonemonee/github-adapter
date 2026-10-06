use super::*;
use engine::{Catalog, Entry, Package};
use std::cell::Cell;
use std::time::Duration;

#[derive(Default)]
struct FakeCatalog {
    packages: Vec<Package<usize>>,
    entries: BTreeMap<usize, Vec<Entry<usize>>>,
    queried: Vec<usize>,
    activated: Vec<usize>,
    fail_packages: bool,
    fail_entries: bool,
    activation: Option<Result<bool>>,
}

impl Catalog for FakeCatalog {
    type Package = usize;
    type Entry = usize;

    fn packages(&mut self) -> Result<Vec<Package<usize>>> {
        if self.fail_packages {
            return Err(failure("fixture_failure", "Package discovery failed."));
        }
        Ok(self.packages.clone())
    }

    fn entries(&mut self, package: &usize) -> Result<Vec<Entry<usize>>> {
        self.queried.push(*package);
        if self.fail_entries {
            return Err(failure("fixture_failure", "App-list discovery failed."));
        }
        Ok(self.entries.get(package).cloned().unwrap_or_default())
    }

    fn activate(&mut self, entry: &usize) -> Result<bool> {
        self.activated.push(*entry);
        self.activation.clone().unwrap_or(Ok(true))
    }
}

fn package(handle: usize, kind: Kind, version: u32) -> Package<usize> {
    let (name, family) = match kind {
        Kind::Codex => (CODEX_NAME, CODEX_FAMILY),
        Kind::ChatGpt => (CHATGPT_NAME, CHATGPT_FAMILY),
    };
    Package {
        handle,
        name: name.into(),
        publisher_id: PUBLISHER_ID.into(),
        family: family.into(),
        registration: format!("{name}_{version}.0.0.0_arm64__{PUBLISHER_ID}"),
        store_signed: true,
        application_package: true,
    }
}

fn entry(handle: usize, family: &str, application: &str, label: Option<&str>) -> Entry<usize> {
    Entry {
        handle,
        target: format!("{family}!{application}"),
        display_name: label.map(str::to_owned),
    }
}

fn codex(label: Option<&str>) -> FakeCatalog {
    FakeCatalog {
        packages: vec![package(1, Kind::Codex, 1)],
        entries: BTreeMap::from([(1, vec![entry(11, CODEX_FAMILY, "App", label)])]),
        ..Default::default()
    }
}

fn apps(catalog: &mut FakeCatalog) -> Vec<DesktopApp> {
    engine::discover(catalog)
        .unwrap()
        .into_iter()
        .map(|entry| entry.app)
        .collect()
}

#[test]
fn codex_identity_survives_chatgpt_or_localized_or_missing_display_metadata() {
    for label in [Some("ChatGPT"), Some("Assistant localisé"), None] {
        let mut catalog = codex(label);
        let discovered = apps(&mut catalog);
        let selected = select("codex", &discovered).unwrap();
        assert_eq!(selected.name, "codex");
        assert_eq!(selected.target, format!("{CODEX_FAMILY}!App"));
        if label == Some("ChatGPT") {
            let alias = select("chatgpt", &discovered).unwrap();
            engine::launch(&alias, &mut catalog).unwrap();
            assert_eq!(catalog.activated, [11]);
        } else {
            assert!(select("chatgpt", &discovered).is_err());
        }
    }
}

#[test]
fn genuine_chatgpt_wins_explicit_selection_without_becoming_codex() {
    let mut catalog = codex(Some("ChatGPT"));
    catalog.packages.push(package(2, Kind::ChatGpt, 9));
    catalog
        .entries
        .insert(2, vec![entry(22, CHATGPT_FAMILY, "App", Some("Autre nom"))]);
    let discovered = apps(&mut catalog);
    let chatgpt = select("chatgpt", &discovered).unwrap();
    assert_eq!(chatgpt.target, format!("{CHATGPT_FAMILY}!App"));
    engine::launch(&chatgpt, &mut catalog).unwrap();
    assert_eq!(catalog.activated, [22]);
    catalog.packages.remove(0);
    assert!(select("codex", &apps(&mut catalog)).is_err());
}

#[test]
fn repeated_records_deduplicate_but_distinct_apps_or_registrations_are_ambiguous() {
    let mut catalog = codex(Some("Codex"));
    catalog.packages.push(catalog.packages[0].clone());
    catalog
        .entries
        .get_mut(&1)
        .unwrap()
        .push(entry(12, CODEX_FAMILY, "App", Some("Codex")));
    assert_eq!(apps(&mut catalog).len(), 1);
    catalog
        .entries
        .get_mut(&1)
        .unwrap()
        .push(entry(13, CODEX_FAMILY, "SecondApp", Some("Codex")));
    assert_eq!(
        select("codex", &apps(&mut catalog)).unwrap_err().code,
        "desktop_ambiguous"
    );
    catalog.packages.push(package(2, Kind::Codex, 2));
    catalog
        .entries
        .insert(2, vec![entry(21, CODEX_FAMILY, "App", Some("Codex"))]);
    assert_eq!(
        engine::discover(&mut catalog).err().unwrap().code,
        "desktop_ambiguous"
    );
    assert!(catalog.activated.is_empty());
}

#[test]
fn publisher_family_signature_and_application_package_are_all_required() {
    for mode in ["publisher", "family", "name", "signature", "resource"] {
        let mut catalog = codex(Some("ChatGPT"));
        let package = &mut catalog.packages[0];
        match mode {
            "publisher" => package.publisher_id = "untrusted".into(),
            "family" => package.family = "OpenAI.Codex_untrusted".into(),
            "name" => package.name = "Lookalike.Codex".into(),
            "signature" => package.store_signed = false,
            _ => package.application_package = false,
        }
        assert!(apps(&mut catalog).is_empty());
        assert!(catalog.queried.is_empty());
        assert!(catalog.activated.is_empty());
    }
}

#[test]
fn oversized_catalogs_and_invalid_registration_metadata_fail_without_activation() {
    let mut catalog = codex(Some("Codex"));
    catalog.packages = vec![package(1, Kind::Codex, 1); engine::MAX_PACKAGES + 1];
    assert_eq!(
        engine::discover(&mut catalog).err().unwrap().code,
        "desktop_catalog_limit"
    );
    assert!(catalog.queried.is_empty());
    catalog.packages = vec![package(1, Kind::Codex, 1)];
    catalog.entries.insert(
        1,
        vec![entry(11, CODEX_FAMILY, "App", None); engine::MAX_ENTRIES + 1],
    );
    assert_eq!(
        engine::discover(&mut catalog).err().unwrap().code,
        "desktop_catalog_limit"
    );
    catalog.packages[0].registration = "fixture-private\ninvalid".into();
    let error = engine::discover(&mut catalog).err().unwrap();
    assert_eq!(error.code, "desktop_catalog_invalid");
    assert!(!error.message.contains("fixture-private"));
    assert!(catalog.activated.is_empty());
}

#[test]
fn launch_revalidates_registration_after_upgrade_uninstall_and_alias_changes() {
    let mut catalog = codex(Some("ChatGPT"));
    let selected = select("codex", &apps(&mut catalog)).unwrap();
    catalog.packages = vec![package(2, Kind::Codex, 300)];
    catalog.entries =
        BTreeMap::from([(2, vec![entry(22, CODEX_FAMILY, "App", Some("Nouveau nom"))])]);
    engine::launch(&selected, &mut catalog).unwrap();
    assert_eq!(catalog.activated, [22]);
    let alias = DesktopApp {
        name: "chatgpt".into(),
        target: selected.target.clone(),
    };
    assert!(engine::launch(&alias, &mut catalog).is_err());
    catalog.packages.clear();
    assert!(engine::launch(&selected, &mut catalog).is_err());
    assert_eq!(catalog.activated, [22]);
}

#[test]
fn invalid_caller_and_native_targets_never_reach_activation() {
    let mut catalog = codex(Some("Codex"));
    for target in [
        "cmd.exe",
        "shell:AppsFolder\\other",
        "https://fixture-private.invalid",
        "OpenAI.Codex_untrusted!App",
    ] {
        let app = DesktopApp {
            name: "codex".into(),
            target: target.into(),
        };
        assert!(engine::launch(&app, &mut catalog).is_err());
    }
    assert!(catalog.queried.is_empty());
    for target in [
        "https://fixture-private.invalid",
        "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0!App",
    ] {
        catalog.entries.get_mut(&1).unwrap()[0].target = target.into();
        let error = engine::discover(&mut catalog).err().unwrap();
        assert!(!error.message.contains("fixture-private"));
        assert_eq!(error.code, "desktop_catalog_invalid");
    }
    assert!(catalog.activated.is_empty());
}

#[test]
fn failures_do_not_retry_or_substitute_and_already_running_ack_is_success() {
    for outcome in [
        Ok(false),
        Err(failure(
            "fixture_native_failure",
            "Native activation failed.",
        )),
        Ok(true),
    ] {
        let mut catalog = codex(Some("Codex"));
        let app = select("codex", &apps(&mut catalog)).unwrap();
        catalog.activation = Some(outcome.clone());
        assert_eq!(
            engine::launch(&app, &mut catalog).is_ok(),
            outcome == Ok(true)
        );
        assert_eq!(catalog.activated, [11]);
    }
    for entries in [false, true] {
        let mut catalog = codex(Some("Codex"));
        catalog.fail_packages = !entries;
        catalog.fail_entries = entries;
        assert!(engine::discover(&mut catalog).is_err());
        assert!(catalog.activated.is_empty());
    }
}

#[test]
fn async_timeout_cancels_once_and_completed_or_failed_states_do_not_poll_forever() {
    let canceled = Cell::new(0);
    let error = wait_for_completion(
        || Ok(0),
        || canceled.set(canceled.get() + 1),
        Duration::ZERO,
    )
    .unwrap_err();
    assert_eq!(error.code, "desktop_timeout");
    assert_eq!(canceled.get(), 1);
    for status in [1, 2, 3] {
        wait_for_completion(
            || Ok(status),
            || panic!("completed operation canceled"),
            Duration::ZERO,
        )
        .unwrap();
    }
    assert!(
        wait_for_completion(
            || Ok(999),
            || panic!("invalid status retried"),
            Duration::ZERO
        )
        .is_err()
    );
}

struct FakeApartment {
    result: i32,
    entered: Cell<usize>,
    exited: Cell<usize>,
}

impl apartment::Api for FakeApartment {
    fn initialize_mta(&self) -> i32 {
        self.entered.set(self.entered.get() + 1);
        self.result
    }
    fn uninitialize(&self) {
        self.exited.set(self.exited.get() + 1);
    }
}

#[test]
fn apartment_ownership_balances_success_including_s_false_but_not_failures() {
    for result in [0, 1, apartment::CHANGED_MODE, 0x80004005_u32 as i32] {
        let api = FakeApartment {
            result,
            entered: Cell::new(0),
            exited: Cell::new(0),
        };
        let guard = apartment::Guard::enter(&api);
        assert_eq!(guard.is_ok(), result >= 0);
        drop(guard);
        assert_eq!(api.entered.get(), 1);
        assert_eq!(api.exited.get(), usize::from(result >= 0));
    }
}

#[test]
fn apartment_cleanup_runs_on_early_operation_error() {
    let api = FakeApartment {
        result: 0,
        entered: Cell::new(0),
        exited: Cell::new(0),
    };
    let operation = || -> Result<()> {
        let _guard = apartment::Guard::enter(&api).map_err(|_| invalid())?;
        Err(missing())
    };
    assert!(operation().is_err());
    assert_eq!(api.exited.get(), 1);
}
