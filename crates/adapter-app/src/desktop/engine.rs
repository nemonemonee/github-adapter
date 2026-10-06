use super::*;

pub(super) const MAX_PACKAGES: usize = 256;
pub(super) const MAX_ENTRIES: usize = 128;

#[derive(Clone)]
pub(super) struct Package<P> {
    pub handle: P,
    pub name: String,
    pub publisher_id: String,
    pub family: String,
    pub registration: String,
    pub store_signed: bool,
    pub application_package: bool,
}

#[derive(Clone)]
pub(super) struct Entry<E> {
    pub handle: E,
    pub target: String,
    pub display_name: Option<String>,
}

pub(super) struct Discovered<E> {
    pub app: DesktopApp,
    pub handle: E,
}

pub(super) trait Catalog {
    type Package;
    type Entry: Clone;

    fn packages(&mut self) -> Result<Vec<Package<Self::Package>>>;
    fn entries(&mut self, package: &Self::Package) -> Result<Vec<Entry<Self::Entry>>>;
    fn activate(&mut self, entry: &Self::Entry) -> Result<bool>;
}

fn official<P>(package: &Package<P>) -> Option<Kind> {
    let kind = family_kind(&package.family)?;
    let name = match kind {
        Kind::Codex => CODEX_NAME,
        Kind::ChatGpt => CHATGPT_NAME,
    };
    (package.name.eq_ignore_ascii_case(name)
        && package.publisher_id.eq_ignore_ascii_case(PUBLISHER_ID)
        && package.store_signed
        && package.application_package)
        .then_some(kind)
}

pub(super) fn discover<C: Catalog>(catalog: &mut C) -> Result<Vec<Discovered<C::Entry>>> {
    let packages = catalog.packages()?;
    if packages.len() > MAX_PACKAGES {
        return Err(failure(
            "desktop_catalog_limit",
            "The native package catalog exceeded its safety limit.",
        ));
    }
    let mut entries = BTreeMap::new();
    let mut registrations = BTreeMap::<String, String>::new();
    for package in packages {
        let Some(kind) = official(&package) else {
            continue;
        };
        if package.registration.is_empty()
            || package.registration.len() > 512
            || package.registration.chars().any(char::is_control)
        {
            return Err(failure(
                "desktop_catalog_invalid",
                "Windows returned invalid package registration metadata.",
            ));
        }
        let listed = catalog.entries(&package.handle)?;
        if listed.len() > MAX_ENTRIES {
            return Err(failure(
                "desktop_catalog_limit",
                "The native app-list catalog exceeded its safety limit.",
            ));
        }
        for entry in listed {
            if target_kind(&entry.target).ok() != Some(kind)
                || !entry
                    .target
                    .split_once('!')
                    .is_some_and(|(family, _)| family.eq_ignore_ascii_case(&package.family))
            {
                return Err(failure(
                    "desktop_catalog_invalid",
                    "Windows returned an app identity outside its verified package.",
                ));
            }
            let target = entry.target.to_ascii_lowercase();
            let registration = package.registration.to_ascii_lowercase();
            if registrations
                .get(&target)
                .is_some_and(|previous| previous != &registration)
            {
                return Err(ambiguous());
            }
            registrations.insert(target.clone(), registration);
            let mut names = vec![kind.name()];
            // This is an additional compatibility alias, never the basis
            // for identifying Codex or an official ChatGPT installation.
            if kind == Kind::Codex
                && entry
                    .display_name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case("ChatGPT"))
            {
                names.push("chatgpt");
            }
            for name in names {
                entries
                    .entry((name, target.clone()))
                    .or_insert_with(|| Discovered {
                        app: DesktopApp {
                            name: name.to_owned(),
                            target: entry.target.clone(),
                        },
                        handle: entry.handle.clone(),
                    });
            }
        }
    }
    Ok(entries.into_values().collect())
}

pub(super) fn launch<C: Catalog>(app: &DesktopApp, catalog: &mut C) -> Result<()> {
    validate(app)?;
    let entries = discover(catalog)?;
    let mut matching = entries.iter().filter(|entry| {
        entry.app.name == app.name && entry.app.target.eq_ignore_ascii_case(&app.target)
    });
    let selected = matching.next().ok_or_else(missing)?;
    if matching.next().is_some() {
        return Err(ambiguous());
    }
    if catalog.activate(&selected.handle)? {
        // Acknowledgement includes activation of an already-running app.
        // No new-process, focus, inference or settings-reload claim follows.
        Ok(())
    } else {
        Err(failure(
            "desktop_activation_failed",
            "Windows did not acknowledge desktop activation. No retry, command-line fallback or installation was attempted.",
        ))
    }
}
