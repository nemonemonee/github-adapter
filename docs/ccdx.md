# CCDX compatibility review

Reviewed on **October 6, 2026**, against the latest published CCDX **v0.9.11**
(October 5).

Sources: [release](https://github.com/DaleXiao/codex-copilot-dx/releases/tag/v0.9.11),
[changes since our v0.7.9 comparison](https://github.com/DaleXiao/codex-copilot-dx/compare/v0.7.9...v0.9.11),
and [pinned implementation](https://github.com/DaleXiao/codex-copilot-dx/tree/cf7c1110b48703903d8438a9209b636acb277eb2).

## Adopted in 0.2.0

| Change | Adapter behavior |
|---|---|
| Future Fast variants | Use the exact advertised, eligible Responses-capable `MODEL-fast` variant. Unsupported priority requests fail. |
| Chat `agent_message` compatibility | Preserve non-human authority through a quoted bridge representation. Native Responses retain their original input. |
| Bounded model-picker refresh | Bound refresh time and label stale display data. Authentication rejection clears the display cache; inference uses fresh metadata. |
| GPT-6.1 compatibility | Use exact model identity and advertised reasoning efforts from the account catalog. |
| Decompression bounds | Cap incoming zstd window memory as well as decoded body bytes. |

These changes are independently implemented in Rust. Account/provider selection,
single-attempt inference by default and usage reporting remain unchanged.

## Already covered

The adapter already has stable stream identities, explicit truncated-stream errors,
Chat finish-reason mapping, complete tool history, genuine encrypted compaction,
bounded redacted failure diagnostics and narrowly opted-in encrypted-state recovery.

## Deferred

CCDX also adds dashboards, themes/languages, usage analytics, timeline surfaces,
additional runtime tuning and richer image-edit/delivery controls. These are outside
this desktop/compatibility release.

CCDX extracts a version-matched capability catalog from the installed macOS Codex
binary and reloads it when the app changes. This adapter continues to synthesize its
personal account catalog from advertised fields. Bundled-catalog extraction needs
validation against the intended Codex versions. MAI retains its supplied/existing
Codex catalog.

Auto-review and startup model order remain separate choices. Real client tool
continuation remains a [release acceptance check](release-readiness.md).
