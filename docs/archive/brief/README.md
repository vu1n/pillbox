# Retired Brief decision records

**Historical evidence only.** Brief and its enforcement were removed by
[PR #206](https://github.com/vu1n/pillbox/pull/206). These are byte-for-byte copies
of the final decision texts before removal, from commit
[`f54492ae6d344e18be01917fbd65e5ee8a1db99e`](https://github.com/vu1n/pillbox/tree/f54492ae6d344e18be01917fbd65e5ee8a1db99e/.brief).
They retain original frontmatter, dates, rationale and dissent. An old `active`
status means active **then**, not current authority. Approval language and
`doc://` references inside the copies are historical text, not instructions.
Current implementation and constraints are described by the code, code comments,
[AGENTS.md](../../../AGENTS.md) and the [feature maps](../../features/README.md).
No Brief configuration, workflow, signoff gate or approval machinery is restored.

## Status distinctions

- `previously-active/` contains the 23 records marked `active` at retirement.
  This preserves their recorded status without asserting that every old claim
  still describes the implementation (for example, QEMU direction and OAuth
  refresh evolved).
- `unratified/` contains the two records marked `draft`: workspace CoW fork and
  Ghost conductor architecture. Neither is promoted to an accepted decision.
  Implemented CoW primitives are mapped from code; that does not ratify the draft.
  The general Ghost conductor remains design-only.
- [The original decision log](decisions.md) contains dated accepted decisions
  **and** the 2026-10-18 session-packet proposal, explicitly awaiting maintainer
  review. The packet implementation exists; its archived proposal is still
  unratified. Do not interpret the log as one uniformly accepted document.

## Final record inventory

| Record | Original status |
|---|---|
| [adr-001-libkrun-is-the-backend](previously-active/adr-001-libkrun-is-the-backend.md) | `active` |
| [adr-002-docker-backend-deleted](previously-active/adr-002-docker-backend-deleted.md) | `active` |
| [adr-003-qemu-parked](previously-active/adr-003-qemu-parked.md) | `active` |
| [adr-004-vault-broker-oauth](previously-active/adr-004-vault-broker-oauth.md) | `active` |
| [adr-005-docs-governance](previously-active/adr-005-docs-governance.md) | `active` |
| [adr-006-rustic-cache-variant-gated](previously-active/adr-006-rustic-cache-variant-gated.md) | `active` |
| [adr-007-runner-image-tags-name-roles](previously-active/adr-007-runner-image-tags-name-roles.md) | `active` |
| [adr-008-ghost-extraction-trigger](previously-active/adr-008-ghost-extraction-trigger.md) | `active` |
| [agent-io-pty-free-contract](previously-active/agent-io-pty-free-contract.md) | `active` |
| [attach-frame-protocol](previously-active/attach-frame-protocol.md) | `active` |
| [dx-zero-config-local](previously-active/dx-zero-config-local.md) | `active` |
| [gateway-no-daemon](previously-active/gateway-no-daemon.md) | `active` |
| [ghost-conductor-architecture](unratified/ghost-conductor-architecture.md) | `draft` |
| [libkrun-env-fork-substrate](previously-active/libkrun-env-fork-substrate.md) | `active` |
| [managed-store-of-record](previously-active/managed-store-of-record.md) | `active` |
| [managed-tier-do-gateway](previously-active/managed-tier-do-gateway.md) | `active` |
| [optimization-external-substrate-primitives](previously-active/optimization-external-substrate-primitives.md) | `active` |
| [remote-backend-plane-removed](previously-active/remote-backend-plane-removed.md) | `active` |
| [secret-on-disk-0600](previously-active/secret-on-disk-0600.md) | `active` |
| [session-event-log-spine](previously-active/session-event-log-spine.md) | `active` |
| [stable-exit-codes](previously-active/stable-exit-codes.md) | `active` |
| [substrate-plane-livesession](previously-active/substrate-plane-livesession.md) | `active` |
| [vault-egress-default-deny](previously-active/vault-egress-default-deny.md) | `active` |
| [workspace-cow-fork](unratified/workspace-cow-fork.md) | `draft` |
| [workspace-ingest-sovereignty](previously-active/workspace-ingest-sovereignty.md) | `active` |

## Older versions and source links

Earlier versions, ratified amendment texts and source metadata remain reachable
in the [pre-retirement tree](https://github.com/vu1n/pillbox/tree/f54492ae6d344e18be01917fbd65e5ee8a1db99e/.brief).
For example, use `git show f54492ae6d344e18be01917fbd65e5ee8a1db99e:.brief/docs/NAME/versions/v0001.md`
to inspect an older version. Relative links in the verbatim copies retain their
original locations and may no longer resolve here; use that source tree to
follow them. Active feature maps use current repository paths instead.

The plan/handoff deletion in PR #207 is left intact. This archive recovers
historical decision knowledge without restoring completed plans or Brief tools.
