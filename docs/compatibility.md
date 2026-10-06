# Compatibility

GitHub Adapter provides a local compatibility endpoint for Codex Desktop, Codex
CLI and supported Claude Code requests. Copilot model access and capabilities
come from the selected account's live catalog.

| API | Support |
|---|---|
| Responses | Native upstream Responses, or supported Chat-to-Responses translation |
| Responses streaming | Incremental native/translated text and tool deltas; stable opaque IDs |
| Responses continuation | Process-local bounded history, scoped to account/model/route |
| Compaction | Genuine encrypted upstream compaction for compatible Responses models |
| Chat Completions | Supported text, images and function-tool history |
| Anthropic Messages | Supported text, attachments, named tools and text tool results |
| Anthropic streaming | Buffered until complete authoritative usage is available |
| Models | Account-filtered catalog; only advertised capabilities/efforts |
| Fast | Exact eligible upstream `MODEL-fast` variant for priority requests |
| WebSocket Responses | Unsupported; HTTP upgrade requests return 426 |

## History

Personal-mode history expires after 30 minutes and is bounded to 128 records and
a 32 MiB serialized-JSON budget. It is held in memory and lost on restart.
`store: false` disables local retention. It does not change upstream retention.
`previous_response_id` continuation requires compatible account, model and route.

## Boundaries

- Personal mode does not emulate background Responses or a remote conversation store.
- Chat translation requires complete tool call/result history. Unsupported encrypted
  state, hosted tools and message content fail rather than being silently discarded.
- Claude-specific thinking budgets/history, non-text tool results, token-count
  endpoints and service-tier selection are not emulated.
- Unknown usage remains unknown. Malformed usage needed for a Claude response fails.
- A selected provider/account remains fixed for the process. Inference errors do
  not cause account/provider failover.

The public Copilot device client and compatibility endpoints are behavioral
integration points, not a stability guarantee from GitHub. Provider changes may
require adapter updates. See [the CCDX comparison](ccdx.md) for upstream tracking.
