# Amendment proposal: agent-io-pty-free-contract

**Decision:** doc://pillbox/agent-io-pty-free-contract@latest#agent-io-pty-free-contract

**Proposed by:** Claude (harness update, PR #174 follow-up)

**Date:** 2026-10-06

## What should change

Name each agent's structured mode as built. Codex's is `app-server` (JSON-RPC over
stdio, the `codex-serve` agent) plus its rollout transcript for the PTY agent;
Claude's is `-p --output-format stream-json` (the `claude-stream` agent) without
hooks. The invariants do not change.

## Why

The decision still names `codex proto`, which Codex removed; pillbox drives
`codex app-server` (`src/sandbox/appserver.rs`, `src/events/codex_serve.rs`) and
parses the rollout (`src/events/transcripts/codex.rs`). It also lists Claude
"hooks", which are not wired. A decision that names a protocol nothing speaks
points the next agent at the wrong code.


---
ratified_rev: 0002
ratified_by: Vu (approved in project thread 2026-10-06: "Amend codex")
