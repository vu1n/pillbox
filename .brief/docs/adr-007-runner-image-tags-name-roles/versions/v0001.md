---
id: adr-007-runner-image-tags-name-roles
project: pillbox
type: decision
status: active
title: Runner image tags name roles, not history
supersedes: docs/decisions.md#ADR-007
related_code:
  - "scripts/build-runner.sh"
  - "src/pillbox.rs"
  - "src/registry.rs"
---

<!-- brief:anchor runner-image-tags-name-roles -->
## Three tag roles — `dev`, `latest`, `vX.Y.Z` — configs point at roles

The runner image has exactly three tag *roles*: `dev` (moving; built locally,
published by CI on merge-to-main), `latest` (moving; CI on stable release, alias
of the newest `vX.Y.Z`, the built-in `DEFAULT_RUNNER_IMAGE`), and `vX.Y.Z`
(immutable, per release). A `pillbox.toml` pins `dev` or `latest`; pin a concrete
`vX.Y.Z` **only** for a reproducible run (a frozen eval / σ̂ baseline).

**Why.** The old `l5`/`l6`/`l7`/`l8` "generation" tags smuggled version control
into Docker tag names, leaked into config/scripts/CI/memory, and churned live —
"which tag is current?" became a research task. Docker tags are pointers, not a
DAG; roles are stable, generation numbers are not.

### Invariant
- Scripts, the `new -i` wizard prefill, and CI default to `dev`;
  `DEFAULT_RUNNER_IMAGE` stays `…:latest`.
- Reproducibility lives only in immutable `vX.Y.Z` tags, never in a moving tag.
- A live *config pointer* to a generation tag (`pillbox-runner:l<N>`) is drift;
  a historical "live-verified (l7)" *note* recording which libkrun dev phase was
  checked is a changelog, not a current-tag pointer, and is fine.
