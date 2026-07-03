---
id: workspace-ingest-sovereignty
project: pillbox
type: decision
status: active
title: Workspace ingest secret-exclusion is non-negotiable (invariant I6)
related_code:
  - "src/workspace/ingest.rs"
  - "src/sandbox/libkrun/session.rs"
---

<!-- brief:anchor workspace-ingest-sovereignty -->
## The secret denylist is pillbox-controlled; the workspace cannot widen it

Before a workspace crosses into a sandbox (CoW clone / virtio-fs share / tar-cp),
`src/workspace/ingest.rs` strips a **pillbox-controlled** secret denylist —
`.ssh/`, `.aws/`, `.gnupg/`, `.env*` (except `.env.example`/`.env.sample`
templates), `*.pem`/`*.key`/`*.p12`, `id_rsa`/`.netrc`/… — and **reports** what
it dropped (`IngestPlan.excluded_secrets`, no silent caps). A file in the
workspace asking to keep a secret (a `.pillboxinclude`) has **zero effect** on
the denylist.

**Why.** This upholds invariant **I6 (sovereignty)**: nothing the user didn't
intend leaves. An untrusted repo must not be able to negate the exclusion and
smuggle host secrets into (or out of) the sandbox — so the denylist is read from
pillbox, never from anything inside the workspace, and it is a hard rule, not a
default a config can override.

### Invariant
- `is_secret_dir` / `is_secret_basename` are pillbox-owned; no workspace file
  can widen egress or un-exclude a secret path.
- Dropped secrets are *reported* (auditable), never silently included or capped.
- Template `.env.example`/`.env.sample` are spared; real `.env*` are excluded.
