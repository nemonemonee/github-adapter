# Everyday use

Use the full CLI path from [Installation](install.md) if `github-adapter` is not
on your PATH.

## Sign in

```sh
github-adapter login
github-adapter account
```

Complete the device sign-in in your browser. Tokens are stored in Windows
Credential Manager or macOS Keychain. The adapter uses its own saved account.

To switch accounts, stop first, run `logout`, then `login`. Start a new coding
conversation after changing account, provider or model.

## Connect Codex

Open GitHub Adapter from its Windows shortcut or Mac app bundle. It discovers
configured providers, selects a preferred available model, starts or reuses a
compatible adapter, temporarily configures Codex, and opens the installed app.

Choose **Quit — restore normal Codex** when finished. The adapter restores the
routing settings it owns while preserving unrelated edits. Start a new Codex
session or reopen Codex afterward; an existing session can retain its old endpoint.

The desktop workflow needs a saved sign-in or a running MAI proxy.

## Choose a provider

```sh
github-adapter start --provider github
github-adapter start --provider mai
github-adapter start --provider auto
```

Automatic selection considers the existing MAI proxy and this adapter's saved
GitHub account. It prefers the configured model order; MAI wins a same-model tie.
Use an explicit provider when the account/data destination matters. A running
server stays bound to its selected account and provider.

## Use the foreground server

```sh
github-adapter github
github-adapter mai --upstream http://127.0.0.1:5000
```

Keep the terminal running. These commands serve without changing client settings.
Add `--setup --client codex`, `--setup --client claude`, or `--setup --client both`
when you want explicit client configuration. Manual setup persists after exit:

```sh
github-adapter restore --client both
```

## Claude Code

```sh
github-adapter github --setup --client claude
```

Open Claude Code separately. Supported Anthropic Messages requests are translated
for the selected upstream model. Streaming replies are buffered until authoritative
usage is available; they do not arrive token by token.

## Models and Fast

```sh
github-adapter models --provider github
```

Available models come from your account's catalog. `--preferred-model MODEL`
can be repeated to supply your own priority order at startup.

Fast is available only when Copilot advertises the exact eligible `MODEL-fast`
variant. Unsupported priority requests fail explicitly. Auto-review resolves to a
compatible Responses model in the same account; see [Configuration](configuration.md).

## Status and stop

```sh
github-adapter status
github-adapter open
github-adapter stop
github-adapter doctor --client both
```

`status`, `open` and `stop` control the desktop/background instance for the current
user. `doctor` inspects client settings and recovery state; it does not prove
Copilot inference is available. `/health` reports local liveness.

[Configuration](configuration.md) · [Troubleshooting](troubleshooting.md)
