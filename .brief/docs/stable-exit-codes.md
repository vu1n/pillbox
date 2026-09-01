---
id: stable-exit-codes
project: pillbox
type: decision
status: active
title: Exit-code categories are a frozen numeric contract
related_code:
  - "src/errors.rs"
---

<!-- brief:anchor stable-exit-codes -->
## `ExitCategory` numeric values are a public contract — never renumber in place

`src/errors.rs` defines a numbered `ExitCategory` (Success=0, Runtime=1, Usage=2,
Config=3, Resource=4) that `PillboxError` maps to. Agents and shell scripts
depend on the numeric values, so they are frozen: **do NOT renumber without a
major version bump.** New categories append; existing numbers never shift.

**Why.** The exit codes are a machine-readable diagnostic API (the named-error-code
analogue). Renumbering silently breaks every script and orchestrator that
branches on `$?` — a backward-incompatible change disguised as a refactor. This
is a discovered decision recorded only as an errors.rs doc-comment.

### Invariant
- The integer value of each existing `ExitCategory` variant is stable across
  minor/patch releases.
- A new category takes the next free integer; it does not reorder the enum.
