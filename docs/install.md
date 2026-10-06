# Installation

GitHub Adapter needs no Python, Node or Rust runtime. Install your coding client
separately and make sure your GitHub account has access to the Copilot models you
want to use.

**0.2.0 preview for personal testing.** A stable release is pending.
See [release readiness](release-readiness.md) for validation and remaining checks.

## Get a test package

1. Sign in to GitHub, open [Test builds](https://github.com/nemonemonee/github-adapter/actions/workflows/verify.yml)
   and choose a successful **Verify candidate** run.
2. Under **Artifacts**, download the artifact for your platform:
   `windows-arm64-candidate`, `windows-x64-candidate`, `macos-arm64-candidate`
   or `macos-x64-candidate`.
3. Extract the downloaded outer artifact ZIP and open its versioned package
   folder. It contains the package ZIP and matching `.zip.sha256` file used below.

These are CI preview artifacts. No GitHub Release assets are created at this step.

## Windows

Requirements: Windows ARM64 or x64 and PowerShell 7. Check **Settings → System →
About → System type** to choose the correct package.

In PowerShell 7, change to the folder containing the Windows package ZIP and its
matching `.zip.sha256` file, then verify and extract it:

```powershell
$zip = '.\github-adapter-0.2.0-windows-x64-unsigned.zip'
$expected = ((Get-Content -LiteralPath "$zip.sha256" -Raw).Trim() -split '\s+')[0]
if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ine $expected) {
    throw 'Checksum mismatch. Download the package again.'
}
Expand-Archive -LiteralPath $zip -DestinationPath .\github-adapter-0.2.0
Set-Location .\github-adapter-0.2.0
pwsh -NoProfile -File .\tools\install-native.ps1 -Source .\bin\github-adapter.exe
```

Replace `x64` with `arm64` for an ARM64 computer. The installer creates a desktop
shortcut and installs both executables under `%LOCALAPPDATA%\GitHubAdapter\bin`.
It does not start the app or sign in.
It restores stopped legacy Codex routing when verified recovery data exists;
add `-NoConfigurationChange` to the installer command to leave client settings unchanged.

Sign in once:

```powershell
& "$env:LOCALAPPDATA\GitHubAdapter\bin\github-adapter.exe" login
```

Open **GitHub Adapter** from the desktop shortcut. Use that full CLI path for
commands, or add its directory to your user PATH yourself.

## macOS

Requirements: macOS 13 or later, Apple Silicon or Intel, and Codex installed.
Native macOS qualification is still pending for this candidate.

Choose `arm64` for Apple Silicon or `x64` for Intel. Verify the downloaded ZIP:

```sh
shasum -a 256 -c github-adapter-0.2.0-macos-arm64-unsigned.zip.sha256
ditto -x -k github-adapter-0.2.0-macos-arm64-unsigned.zip ./github-adapter-0.2.0
```

Move **GitHub Adapter.app** into `/Applications` or your `~/Applications` folder.
Keep the bundle intact; its CLI, host, recovery helper and resources work together.

Sign in from Terminal. If you installed in `~/Applications`, replace
`/Applications` with `$HOME/Applications` in these commands.

```sh
"/Applications/GitHub Adapter.app/Contents/MacOS/github-adapter" login
```

Open **GitHub Adapter.app** in Finder. Its menu-bar menu provides Status, Open
Codex and Quit. Terminal commands use the same quoted CLI path. Installation does
not modify your shell profile or install an always-running login service.

## Package trust

A checksum verifies that the bytes match the supplied digest. Get both from the
same trusted build or release channel. Package filenames state `signed` or `unsigned`;
unsigned candidates do not provide a verified publisher identity.
Mac unsigned candidates carry an ad-hoc integrity signature required by Apple
Silicon; this does not establish a trusted publisher.

Follow the operating system's normal installation and trust controls. If the
package is blocked on a managed computer, use its approved software process.

## Update

1. Quit the adapter and confirm `github-adapter status` reports it stopped.
2. If images are enabled, close Codex so its image helper releases the executable.
3. Verify the new package.
4. On Windows, run the new bundled installer. On Mac, replace the complete app bundle.
5. Open GitHub Adapter and check `--version` and the selected account/model.

Saved sign-in and original settings backups remain in their existing namespaces.
Use `logout` before changing to another GitHub account.

## Remove

Quit first. If you used manual `setup`, run `restore --client both` for the clients
you configured. Run `image-disable` if optional images were enabled, then `logout`
if you also want to remove the saved GitHub credential.

On Windows, remove the GitHub Adapter desktop shortcut and its installed
`%LOCALAPPDATA%\GitHubAdapter\bin` directory. On Mac, remove GitHub Adapter.app.
Retain configuration backups until you have verified your normal client settings.

[Next: everyday use](usage.md)
