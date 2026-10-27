---
id: adr-005-docs-governance
project: pillbox
type: decision
status: active
title: Docs are a router + one canonical doc per subsystem + an ADR log
supersedes: docs/decisions.md#ADR-005
related_code:
  - "AGENTS.md"
  - "docs/README.md"
  - "docs/decisions.md"
---

<!-- brief:anchor docs-governance -->
## Curate docs into a router, canonical-per-subsystem docs, and an ADR log

AGENTS.md is a slim **router** (mental model + subsystem map + links). Each live
subsystem has ONE canonical doc marked with a STATUS/verified-date header.
Planning/research/superseded docs move to `docs/archive/` and are NOT
authoritative. The load-bearing decisions live in the ADR log (`docs/decisions.md`,
now mirrored as governed `adr-*` decisions here).

**Why.** The failure mode is an agent answering authoritatively on partial or
stale context; sprawling docs with no current-vs-aspirational signal made
retrieval unreliable, so the agent guessed. The fix is curation + a trust
contract, not more prose.

### Invariant
- **Code wins over doc:** when a canonical doc and the code disagree, fix the doc
  in the same change (this backfill's re-grounding notes are that discipline
  applied retroactively).
- Read a subsystem's canonical doc before acting on or claiming things about it.
- Archived docs are reference/history, never a source of current truth.
