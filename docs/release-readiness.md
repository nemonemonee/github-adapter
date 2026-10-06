# 0.2.0 preview

**0.2.0 preview for personal testing.** A stable release is pending.
Repository: [`nemonemonee/github-adapter`](https://github.com/nemonemonee/github-adapter).

This preview adds native macOS integration, current CCDX compatibility updates,
the approved Black Cat Cut-out icon, licensing, product documentation and a four-platform CI matrix.

## Evidence

| Check | Candidate result |
|---|---|
| Native Windows ARM64 and x64 workspace tests | 364 passed, 0 failed, 8 ignored on each architecture |
| Native Windows ARM64 and x64 strict Clippy | Passed |
| Rust formatting | Passed |
| Product documentation links | 12 Markdown files verified |
| Locked dependency notices | Verified |
| Windows icon contracts | 559 assertions passed on ARM64; approved hashes and frame structure verified |
| Windows release binaries | ARM64 and x64 built with static CRT |
| Windows installer/archive contracts | 109 passed, 0 failed on native ARM64 |
| Native macOS Apple Silicon and Intel workspace tests | 318 passed, 0 failed, 1 ignored on each architecture |
| Native macOS Apple Silicon and Intel strict Clippy | Passed |
| Native macOS release bundles | Both architectures built; ad-hoc signatures and extracted packages verified |

Native macOS CI compiles, links, tests and verifies packages on
[Apple Silicon](https://github.com/nemonemonee/github-adapter/actions/runs/37416581273/job/112116412852)
and [Intel](https://github.com/nemonemonee/github-adapter/actions/runs/37416581273/job/112116413376).
The ignored Mac test is an opt-in Keychain round trip that still needs a receiving Mac.
[Test builds](https://github.com/nemonemonee/github-adapter/actions/workflows/verify.yml)
provide preview packages after successful runs.

## Before a stable release

- Receiving-computer installation and updates on each intended platform.
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
