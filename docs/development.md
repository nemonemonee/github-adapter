# Development

The workspace has three crates:

| Crate | Responsibility |
|---|---|
| `adapter-protocol` | Request, resource, usage, streaming and conversion contracts |
| `adapter-runtime` | Authentication, providers, HTTP, model catalogs and history |
| `adapter-app` | CLI, credentials, client configuration and native desktop lifecycle |

Use the exact Rust version in `rust-toolchain.toml`. `Cargo.lock` is committed.
Python/Node are unnecessary to run the product; Python 3 is used by documentation
and license-maintenance tools during development.

## Windows

Install Rust and Visual Studio C++ Build Tools with MSVC, the Windows SDK, x64
tools and ARM64 tools as needed. Use PowerShell 7.

```powershell
.\tools\build-windows.ps1 -Action test -Locked
.\tools\build-windows.ps1 -Action clippy -Locked
cargo +1.98.1 fmt --all -- --check
.\tools\build-windows.ps1 -Action build -Package adapter-app -Release -StaticRuntime -Locked
.\tools\test-native.ps1
```

`-Architecture x64` or `arm64` selects the target. Add the corresponding Rust
target with `rustup target add --toolchain 1.98.1 TARGET`. Cross-architecture
execution requires `-AllowEmulation`; record it as emulation, not native validation.

## macOS

Install the Xcode Command Line Tools and the pinned Rust toolchain. Build and test
on the native Apple Silicon or Intel architecture:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
bash tools/build-macos.sh
bash tools/package-macos.sh --unsigned
```

The package script verifies the app bundle, architecture, version and archive.
The minimum is macOS 13 (`MACOSX_DEPLOYMENT_TARGET=13.0`). Cross-builds still need
native execution on the target architecture.

Use `bash tools/package-macos.sh --identity 'Developer ID Application: ...'`
for a Developer ID signed candidate. With an existing notarytool keychain profile,
add `--notary-profile PROFILE` to notarize and staple it before final packaging.
Signing credentials are supplied locally; they are not stored in source.

## Windows packages

Copy the freshly built CLI and host into a new preparation directory so they are
independent files rather than Cargo hard links, then package:

```powershell
New-Item -ItemType Directory .\artifacts\prepare-arm64
Copy-Item -LiteralPath .\target\aarch64-pc-windows-msvc\release\github-adapter.exe -Destination .\artifacts\prepare-arm64\
Copy-Item -LiteralPath .\target\aarch64-pc-windows-msvc\release\github-adapter-host.exe -Destination .\artifacts\prepare-arm64\
.\tools\package-native.ps1 -Architecture arm64 -Source .\artifacts\prepare-arm64\github-adapter.exe -Unsigned
```

Use x64 paths and `-Architecture x64` for Intel Windows. Output paths must be new
directories below `artifacts`. The verifier checks archive contents, payload
hashes, versions, architecture, subsystems and embedded artwork.

## Artwork

The original connector mark lives in `assets/github-adapter-dark.svg` with its
1024-pixel RGBA PNG. Windows resources use generated application/tray ICOs;
Mac uses `app.icns` and a monochrome template PNG.

```powershell
.\tools\generate-icon.ps1 -Check
.\tools\tests\test-icons.ps1
```

The default icon check verifies exact hashes of the approved source and ICOs.
Windows GDI+ can resample pixels differently across systems, so packages use the
committed exports. Fixture checks with explicit source or output paths compare
regeneration on the same machine.

After changing artwork, regenerate platform assets, review the exports, update
the approved hashes in the icon and native qualification tools, and rebuild both
executables.

## Maintenance checks

```sh
python tools/check-docs.py
python tools/update-notices.py --check
```

Run the notice tool without `--check` after changing dependencies. It reads locked
Cargo metadata and copies dependency license texts.

Tests use isolated client profiles and synthetic providers. Native credential
interoperability tests are explicit opt-ins. Do not use real client settings or
send paid inference/image requests as part of the default suite.

## Before a stable release

Complete the native and live-client checks in [release readiness](release-readiness.md).
CI retains [test build artifacts](https://github.com/nemonemonee/github-adapter/actions/workflows/verify.yml).
Review personal testing results before the final release.
