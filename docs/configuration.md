# Configuration

Most users only need `login` and the desktop app. Use CLI options for explicit
provider selection, custom paths or foreground operation.

## Files and credentials

| Setting | Default |
|---|---|
| Codex settings | `~/.codex/config.toml` |
| Claude settings | `~/.claude/settings.json` |
| Windows recovery data | `%LOCALAPPDATA%/GitHubAdapter/client-backups` |
| macOS recovery data | `~/Library/Application Support/GitHubAdapter/client-backups` |
| Server | `http://127.0.0.1:5001` |
| Optional MAI upstream | `http://127.0.0.1:5000` |

`CODEX_HOME` and `CLAUDE_CONFIG_DIR` override the corresponding client directories.
`--codex-config`, `--claude-settings` and `--backup-dir` select explicit paths.

Credentials use the existing `MAI Adapter` / `personal-github` service/account
namespace for upgrade compatibility. That internal name does not select MAI.
Optional image credentials are separate from the coding account.

## Temporary and manual setup

The desktop app temporarily changes Codex routing and restores it on Quit.
A recovery companion handles crashes; cleanup at the next OS login handles an
interrupted process tree or power loss. Recovery verifies the owning process and
preserves conflicting user edits. Use `doctor` and `recover` to inspect interrupted changes.

Explicit CLI `setup` or `--setup` uses persistent setup. Run `restore` when finished.

```sh
github-adapter setup --client codex --dry-run
github-adapter restore --client codex --dry-run
github-adapter recover --client codex --dry-run
```

`--dry-run` previews changes without applying them.

## Account and model overrides

| Option or environment variable | Purpose |
|---|---|
| `--provider github\|mai\|auto` | Select the provider |
| `--github-account LOGIN` | Require the saved account to match |
| `--preferred-model MODEL` | Supply model priority; repeat for alternatives |
| `--models-cache PATH` | Select a Codex catalog for MAI compatibility |
| `--upstream URL` | Select the MAI proxy |
| `--port PORT` | Choose another loopback port for foreground serving |
| `GITHUB_ADAPTER_TOKEN` | Unattended GitHub OAuth token override |
| `GITHUB_ADAPTER_CLIENT_ID` | OAuth device client ID override |
| `GITHUB_ADAPTER_REVIEW_MODEL` | Auto-review Responses model |
| `GITHUB_ADAPTER_RECOVER_ENCRYPTED_STATE=1` | Opt into bounded same-binding recovery |

Use `login --token` for hidden token entry. Avoid putting secrets in command arguments.
The legacy `MAI_ADAPTER_GITHUB_TOKEN` and `MAI_ADAPTER_GITHUB_CLIENT_ID` aliases remain
accepted. Conflicting primary/legacy overrides fail instead of choosing an account.

Encrypted-state recovery stays off by default. When enabled, it permits one
narrowly eligible retry before visible output, with the same account/model/route
and original deadline. Authentication failures, rate limits and ambiguous output
are not replayed.

## Local access

Inference is loopback-only and rejects external browser origins. Local processes
can still send requests. Host management is restricted to the current user.
