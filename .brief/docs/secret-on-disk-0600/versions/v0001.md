---
id: secret-on-disk-0600
project: pillbox
type: decision
status: active
title: Secret-on-disk = 0600, enforced by one helper
related_code:
  - "src/paths.rs"
  - "src/workspace/rustic.rs"
  - "src/commands/workspace.rs"
  - "src/vault/ca.rs"
---

<!-- brief:anchor secret-on-disk-0600 -->
## Every private file pillbox writes goes through the 0600 write helper

Any file pillbox writes that holds a secret (creds, rustic repo password, CA
key, temp password materializations) is created with mode `0600` via the
centralized helper in `src/paths.rs` (`write_private_file` / `append_private_file`).
Callers do not hand-roll `OpenOptions`.

**Why.** "secret-on-disk = 0600" is a security invariant that silently rots if
each call site re-implements it — one caller forgetting `.mode(0o600)` is a
world-readable credential. Keeping the invariant in one place makes it auditable
and impossible to drift per-caller. This is a discovered decision: it lives only
as a `paths.rs` doc-comment and the convergent call sites, with no ADR.

### Invariant
- New secret-file writes call the `paths.rs` private-file helper; they do not
  open files with raw `OpenOptions` + a local `.mode()`.
- The per-pillbox rustic password, the vault CA key, and any temp
  password/creds materialization are all 0600 through this one path.
