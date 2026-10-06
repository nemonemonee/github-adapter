<p align="center"><img src="assets/github-adapter-dark.png" alt="GitHub Adapter" width="96"></p>

# GitHub Adapter

Use **Codex and Claude Code with your GitHub Copilot account**.

GitHub Adapter connects your coding client to Copilot and stores account
credentials in Windows Credential Manager or macOS Keychain.
Open the desktop app to connect Codex. Quit it to restore your normal settings.

**0.2.0 preview for personal testing.** A stable release is pending. Windows and
macOS source are included; native desktop and live-client validation remain open.

[Install](docs/install.md) · [Everyday use](docs/usage.md) ·
[Troubleshooting](docs/troubleshooting.md) · [Build from source](docs/development.md) ·
[Test builds](https://github.com/nemonemonee/github-adapter/actions/workflows/verify.yml) ·
[All guides](docs/README.md)

## What you get

- A native desktop app and CLI, with no Python or Node runtime requirement.
- GitHub device sign-in and account checks.
- A local Responses, Chat Completions, and supported Anthropic Messages endpoint.
- Codex settings that return to normal after Quit, with protection for your edits.
- Live account model discovery and Fast mode when Copilot advertises an eligible variant.
- Optional image generation through a separately configured provider.

An existing MAI proxy can also be used. Personal Copilot works independently of MAI.

## Get started

You need Codex or Claude Code installed separately, a GitHub account with access
to the desired Copilot models, and a package for your computer.

1. [Install the adapter](docs/install.md).
2. Run `github-adapter login` and complete GitHub's device sign-in.
3. Open **GitHub Adapter**. It connects Codex and opens the app.
4. Choose **Quit — restore normal Codex** when you are finished.

The [installation guide](docs/install.md) shows the CLI path for each platform.
See [Everyday use](docs/usage.md) for foreground serving, Claude Code, provider
selection and restoring settings after manual setup.

## Platforms

| Platform | Packages | Credential storage |
|---|---|---|
| Windows | ARM64 and x64 ZIPs | Windows Credential Manager |
| macOS | Apple Silicon and Intel app bundles | macOS Keychain |

Packages include checksums and license notices; filenames show their signing
status. See [release readiness](docs/release-readiness.md) for candidate validation.

## Your account and data

Personal mode sends prompts, source snippets, attachments and tool results to
GitHub Copilot through the selected account. Select MAI explicitly for work that
must use that provider. Automatic mode can choose either configured provider.

The adapter listens on loopback at `127.0.0.1:5001`. Other local processes can use
its inference endpoint. Management commands are restricted to the current user.

Copilot compatibility endpoints can change. Available models and capabilities
depend on your account. See [Compatibility](docs/compatibility.md) for supported
protocols and limits, including buffered Claude streaming.

## License and acknowledgements

[MIT](LICENSE). Dependency licenses are included in [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt).
Protocol research and upstream references are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

GitHub Adapter is an independent project. It is not affiliated with GitHub,
OpenAI or Anthropic. The icon combines GitHub and OpenAI brand marks, which
remain the property of their respective owners. See the [artwork notice](THIRD_PARTY_NOTICES.md#artwork).
