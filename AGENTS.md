# mpv-targets v1 implementation guidance

Read `SPEC.md` completely before making any design, implementation, protocol,
client, or deployment decision. It is the canonical v1 contract.

## Rewrite discipline

- This is a greenfield implementation. Do not copy source from the abandoned
  worktrees.
- The old implementations are behavioral evidence only. Recreate proven
  behavior through the acceptance matrix; do not preserve their structure,
  naming, or accidental concepts.
- Do not change a public command, protocol field, lifecycle rule, or answer-file
  contract without an explicit `SPEC.md` amendment approved by the user.
- Every mutating operation must provide a correlated terminal result. Eventual
  mpv/media failures remain separate observable state.

## Product boundary

- `mpv-targets` is named-target supervision and control for mpv: just the
  streams.
- mpv is the executor. Do not add media-library, discovery, downloader, NVR,
  sender-UI, generic-player, account, PKI, or fleet-control features.
- PaneBot and every other client use the same public `mpv_targets` library and
  protocol. No private endpoints, shared runtime state, or remote filesystem
  shortcuts.
- Hardware profiles belong to deployment. The daemon and public protocol stay
  generic across Linux hosts.

## Implementation checks

- Implement against the acceptance matrix in `SPEC.md` from the first module.
- Preserve disabled, stopped, recovery, rename, and no-duplicate-process
  semantics exactly as specified.
- Keep `targets` a non-interactive operator client. It uses the public library
  only and does not grow into PaneBot.
- Keep deployment non-interactive and non-destructive by default.
