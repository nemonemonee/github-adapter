# 0.2.0 preview

**0.2.0 preview for personal testing.** A stable release is pending.
Target repository: [`nemonemonee/github-adapter`](https://github.com/nemonemonee/github-adapter).

This preview adds native macOS integration, current CCDX compatibility updates,
original artwork, licensing, product documentation and a four-platform CI matrix.

## Evidence

| Check | Candidate result |
|---|---|
| Windows ARM64 workspace tests | 364 passed, 0 failed, 8 ignored |
| Windows ARM64 strict Clippy | Passed |
| Rust formatting | Passed |
| Product documentation links | 12 Markdown files verified |
| Locked dependency notices | Verified |
| Windows icon contracts | 444 assertions passed; regeneration matches |
| Windows release binaries | ARM64 and x64 built with static CRT |
| Windows installer/archive contracts | 109 passed, 0 failed on native ARM64 |
| macOS source checks | Workspace/all-target checks and strict Clippy passed for Apple Silicon and Intel |

The Mac checks cross-compile Rust metadata and zstd C objects from Windows using
verified Zig 0.17.0 and real Darwin dependencies. Native linking, tests and Finder,
Keychain, APFS, LaunchAgent and signing behavior remain untested by these checks.
Native CI [test builds](https://github.com/nemonemonee/github-adapter/actions/workflows/verify.yml)
can provide preview packages after successful runs.

## Before a stable release

- Native macOS Apple Silicon and Intel compilation, tests and package verification.
- Native Windows x64 tests and installer qualification; ARM64 desktop/client checks below.
- Finder/Explorer launch, compatible-instance reuse, port collision, clean Quit,
  crash recovery, interrupted login recovery, sleep/resume and updates.
- Personal login/logout, model discovery, a real Codex tool round trip/compaction,
  and supported Claude Code requests on the intended client versions.
- Review of the license, artwork, dependency notices, docs and clean source snapshot.
- A decision on signed distribution versus explicitly labelled unsigned preview packages.
- Review of personal testing results before the final release.

Mac discovery checks the official `com.openai.codex` bundle identifier and the
OpenAI signing team `2DC432GLL2`. The bundle identifier is publisher documented;
the team identifier is inferred from OpenAI's published `com.openai.chat` app
association. Verify the current Codex app's TeamIdentifier on a receiving Mac
before qualification. Discovery rejects a different signer. References:
[OpenAI desktop policy](https://help.openai.com/en/articles/20001535-manage-chatgpt-desktop-browser-policies-with-mdm),
[OpenAI app association](https://openai.com/.well-known/apple-app-site-association).

On the receiving Mac, inspect the installed Codex bundle's `Identifier` and
`TeamIdentifier`:

```sh
codesign --display --verbose=4 "/Applications/Codex.app" 2>&1
```

Use its actual installed path; the unified app may be named `ChatGPT.app`.
Open GitHub Adapter from Finder and verify its menu, visible startup errors and
Quit restoration. CI builds and package checks do not exercise that interaction.

Automated tests use synthetic providers. Live Copilot access and desktop/client
behavior still need the checks above; soak tests alone do not qualify a release.
