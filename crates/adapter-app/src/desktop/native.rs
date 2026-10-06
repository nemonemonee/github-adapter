use super::*;
use engine::{Entry, Package as PackageRecord};
use std::time::Duration;
use windows::ApplicationModel::{Core::AppListEntry, Package, PackageSignatureKind};
use windows::Management::Deployment::PackageManager;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::HSTRING;

const ASYNC_BUDGET: Duration = Duration::from_secs(15);

fn hresult(operation: &str, code: i32) -> AdapterError {
    failure(
        "desktop_native_error",
        &format!(
            "Windows could not {operation} (HRESULT 0x{:08X}). No fallback was attempted.",
            code as u32
        ),
    )
}

fn checked<T>(operation: &str, value: windows::core::Result<T>) -> Result<T> {
    value.map_err(|error| hresult(operation, error.code().0))
}

struct ApartmentApi;

impl apartment::Api for ApartmentApi {
    fn initialize_mta(&self) -> i32 {
        // windows maps both successful HRESULTs to Ok; both require a
        // matching RoUninitialize, handled by the owned guard.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_or_else(|error| error.code().0, |()| 0)
    }

    fn uninitialize(&self) {
        unsafe { RoUninitialize() };
    }
}

pub(super) fn on_mta<T: Send>(operation: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    let api = ApartmentApi;
    match apartment::Guard::enter(&api) {
        Ok(_guard) => operation(),
        Err(apartment::CHANGED_MODE) => {
            // Never uninitialize or change a caller-owned STA. All WinRT
            // objects are created and dropped within this new MTA.
            std::thread::scope(|scope| {
                let worker = std::thread::Builder::new()
                    .name("adapter-desktop-mta".into())
                    .spawn_scoped(scope, move || {
                        let api = ApartmentApi;
                        let _guard = apartment::Guard::enter(&api)
                            .map_err(|code| hresult("initialize the desktop apartment", code))?;
                        operation()
                    })
                    .map_err(|_| {
                        failure(
                            "desktop_thread_error",
                            "The desktop MTA worker could not be created.",
                        )
                    })?;
                worker.join().map_err(|_| {
                    failure(
                        "desktop_thread_error",
                        "The desktop MTA worker did not complete.",
                    )
                })?
            })
        }
        Err(code) => Err(hresult("initialize the desktop apartment", code)),
    }
}

pub(super) struct Catalog {
    manager: PackageManager,
}

impl Catalog {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            manager: checked("open current-user package discovery", PackageManager::new())?,
        })
    }
}

impl engine::Catalog for Catalog {
    type Package = Package;
    type Entry = AppListEntry;

    fn packages(&mut self) -> Result<Vec<PackageRecord<Package>>> {
        let mut result = Vec::new();
        for family in [CODEX_FAMILY, CHATGPT_FAMILY] {
            // Empty SID means the current user. Family-scoped queries do
            // not enumerate other users or unrelated installed software.
            let packages = checked(
                "enumerate current-user OpenAI packages",
                self.manager.FindPackagesByUserSecurityIdPackageFamilyName(
                    &HSTRING::new(),
                    &HSTRING::from(family),
                ),
            )?;
            let iterator = checked("open the package iterator", packages.First())?;
            while checked("read package iterator state", iterator.HasCurrent())? {
                if result.len() >= engine::MAX_PACKAGES {
                    return Err(failure(
                        "desktop_catalog_limit",
                        "The native package catalog exceeded its safety limit.",
                    ));
                }
                let package = checked("read the installed package", iterator.Current())?;
                let id = checked("read package identity", package.Id())?;
                let application_package =
                    !checked("inspect package framework metadata", package.IsFramework())?
                        && !checked(
                            "inspect package resource metadata",
                            package.IsResourcePackage(),
                        )?
                        && !checked("inspect package bundle metadata", package.IsBundle())?;
                result.push(PackageRecord {
                    name: checked("read package name", id.Name())?.to_string(),
                    publisher_id: checked("read package publisher identity", id.PublisherId())?
                        .to_string(),
                    family: checked("read package family", id.FamilyName())?.to_string(),
                    registration: checked("read package registration", id.FullName())?.to_string(),
                    store_signed: checked(
                        "verify package signature kind",
                        package.SignatureKind(),
                    )? == PackageSignatureKind::Store,
                    application_package,
                    handle: package,
                });
                checked("advance the package iterator", iterator.MoveNext())?;
            }
        }
        Ok(result)
    }

    fn entries(&mut self, package: &Package) -> Result<Vec<Entry<AppListEntry>>> {
        let operation = checked(
            "read registered application entries",
            package.GetAppListEntriesAsync(),
        )?;
        wait_for_completion(
            || checked("read app-list completion state", operation.Status()).map(|status| status.0),
            || {
                let _ = operation.Cancel();
            },
            ASYNC_BUDGET,
        )?;
        let entries = checked("finish app-list discovery", operation.GetResults())?;
        let size = checked("read app-list size", entries.Size())?;
        if size as usize > engine::MAX_ENTRIES {
            return Err(failure(
                "desktop_catalog_limit",
                "The native app-list catalog exceeded its safety limit.",
            ));
        }
        let mut result = Vec::new();
        for index in 0..size {
            let entry = checked("read registered application entry", entries.GetAt(index))?;
            let target = checked("read application identity", entry.AppUserModelId())?.to_string();
            // Display metadata is only an optional legacy alias hint.
            let display_name = entry
                .DisplayInfo()
                .and_then(|info| info.DisplayName())
                .ok()
                .map(|name| name.to_string());
            result.push(Entry {
                handle: entry,
                target,
                display_name,
            });
        }
        Ok(result)
    }

    fn activate(&mut self, entry: &AppListEntry) -> Result<bool> {
        let operation = checked("request desktop activation", entry.LaunchAsync())?;
        wait_for_completion(
            || {
                checked("read activation completion state", operation.Status())
                    .map(|status| status.0)
            },
            || {
                let _ = operation.Cancel();
            },
            ASYNC_BUDGET,
        )?;
        checked("acknowledge desktop activation", operation.GetResults())
    }
}
