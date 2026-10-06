# Troubleshooting

Start with:

```sh
github-adapter --version
github-adapter account
github-adapter status
github-adapter doctor --client both
```

Use the full installed CLI path from [Installation](install.md) if needed.
Copy the error category and the relevant command output when reporting a problem;
leave out tokens, prompts, private code and recovery-file contents.

## Startup says no provider or model is available

Run `account`, then `models --provider github`. Complete `login` if no adapter
account is saved. A successful device login does not guarantee model entitlement.
Check the account's Copilot access and select an advertised model.

For MAI, confirm its configured proxy is running and the selected Codex model
cache is available. Try an explicit provider to identify which path is failing.

## The port is occupied

Check `status`. A compatible instance is reused; an incompatible instance must
be stopped before starting the new version. If another application owns port
5001, leave it running and choose another port for a foreground server.

## Codex cannot be found

Run `github-adapter apps`. Install Codex separately through its normal channel.
On Mac, discovery verifies bundle identity and publisher signature. Multiple
distinct matching installations must be resolved before automatic launch.

The unified Codex app can be installed as `/Applications/ChatGPT.app` (or under
`~/Applications`) with bundle ID `com.openai.codex`. Its filename does not determine
its identity; discovery already checks both `Codex.app` and `ChatGPT.app`. Do not
rename or reinstall a valid app just to match the filename.

Use the latest successful test build. Follow the
[receiving-Mac checks](release-readiness.md) to verify the installed app and signer.

## Settings changed while the adapter was running

Quit normally, then run `doctor`. Recovery preserves unrelated edits and does
not overwrite a conflicting routing edit. Inspect the original configuration
and reported recovery state before deciding which routing settings to keep.

Preview interrupted-operation recovery:

```sh
github-adapter recover --client codex --dry-run
```

## Codex still uses the adapter after Quit

Start a new session or reopen Codex. Existing sessions can retain their loaded
endpoint. If you used manual `setup`, run `restore --client codex` explicitly.

## Credentials are unavailable

Unlock the macOS login Keychain or check access to Windows Credential Manager.
Use `account` to verify the selected login. Stop before `logout` and a new `login`.
The adapter does not fall back to a plaintext token file.

After an unsigned macOS update, run `account` to approve the new executable's
access to the saved sign-in before starting the app. Keychain may identify
`github-adapter` and the legacy `MAI Adapter` credential service. Enter any login
Keychain password in the macOS dialog. If access is denied, leave the app stopped
until you can authorize it.

## Native requests fail behind a proxy

If the adapter cannot connect through your configured proxy, pass its HTTP
endpoint explicitly. Replace `http://127.0.0.1:8080` with your proxy's address:

```sh
HTTPS_PROXY="http://127.0.0.1:8080" \
HTTP_PROXY="http://127.0.0.1:8080" \
NO_PROXY="${NO_PROXY:+$NO_PROXY,}localhost,127.0.0.1,::1" \
  "/Applications/GitHub Adapter.app/Contents/MacOS/github-adapter" models --provider github
```

`NO_PROXY` keeps local adapter/client connections direct. Quit any running adapter,
then use the same environment with `start` instead of `models --provider github`.
Finder does not inherit Terminal variables. Keep any existing state-directory
overrides such as `XDG_STATE_HOME` when running these commands.

## A stream ends or a request fails

Check the upstream error and account/model access. The adapter reports truncated
streams and unsupported request shapes as errors. Failed inference is not
automatically resent through another account or provider.

## Image tools are missing

Run `image-status --check`, then `image-enable` without provider options to repair
existing owned registration. Restart Codex once if it retained an older tool/skill
catalog. See [Images](images.md).

## An installation is blocked

Check the package's signing status and use the operating system's normal trust
process. Managed computers may require an approved distribution path.
