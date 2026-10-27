---
id: text-execution
paths:
  - "src/commands/text.rs"
  - "src/execution/text.rs"
  - "src/execution/text_v2.rs"
  - "src/execution/text_opencode.rs"
  - "src/execution/pi_text.rs"
  - "src/execution/claude_text.rs"
  - "src/execution/claude_text/**"
  - "src/execution/opencode.rs"
  - "src/execution/store.rs"
  - "src/sandbox/libkrun/text_harness.rs"
  - "scripts/smoke/test_claude_text_bridge.py"
  - "scripts/smoke/claude-text-offline.py"
entry:
  - "src/commands/text.rs:execute"
  - "src/execution/text_v2.rs:TextRequestV2"
  - "src/execution/store.rs:InvocationStore"
tests:
  - "src/commands/text.rs:text_claim_reuses_exact_request_and_interrupts_lost_owner"
  - "src/execution/text_v2.rs:a_malformed_request_is_refused_before_any_claim"
  - "scripts/smoke/test_claude_text_bridge.py"
---

# Sealed text turns across harnesses

Callers request one bounded, tool-free text turn, then inspect or cancel its durable invocation. Text/2 selects a harness/model while Pillbox resolves runtime details and returns bounded result evidence. See [text command contracts](../commands.md#sealed-local-text-execution).

## Sub-features

- Text/1’s pinned Codex profile and harness-agnostic text/2 admission.
- Codex, Claude Code, OpenCode, pi and prime-agent drivers.
- Shared no-resampling store, closed failure codes/stages and resolved runtime evidence.

## How to get to it (user POV)

`pillbox text execute --request request.json` admits a sealed text request and runs foreground on libkrun. `pillbox text status ID` reads the original state; `pillbox text cancel ID` records cancellation intent. Text requests do not grant repository access.

## Driving it with the repository harness

`python3 scripts/smoke/test_claude_text_bridge.py` checks Claude bridge fixtures and failure handling offline. With libkrun libraries installed, `cargo test --features libkrun execution::text_v2::tests` checks admission, resolved selection and typed failures; `cargo test --features libkrun commands::text::tests` checks exact retry and owner-loss behavior. None of those commands establishes live provider/model availability.

## Gotchas

- Text/2 rejects caller-supplied harness version, runner image, credentials or egress host before admission. These are runtime-owned selections.
- A requested model or reasoning variant cannot silently fall back to another one.
- Tool-free is an execution contract; a harness tool call or permission request fails the turn rather than widening authority.
