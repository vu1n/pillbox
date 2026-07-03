---
id: adr-006-rustic-cache-variant-gated
project: pillbox
type: decision
status: active
title: Rustic cache is variant-gated — off for Local, on for S3
supersedes: docs/decisions.md#ADR-006
related_code:
  - "src/workspace/rustic.rs"
---

<!-- brief:anchor rustic-cache-variant-gated -->
## `repo_opts` sets the rustic cache per variant, not a blanket setting

`RusticBackend::repo_opts` sets the rustic cache per variant: `no_cache(true)`
for `RusticVariant::Local`, and cache **on** for `RusticVariant::S3` with
`cache_dir` anchored at `<state-dir>/cache`. Do **not** revert either half to a
blanket setting.

**Why.** The original blanket `no_cache(true)` was a local-first default never
differentiated when the S3 variant arrived. For S3 it is a perf footgun — every
host-side open (push/pull/list) re-fetches the index from the bucket — and
rustic's cache is content-addressed + immutable (keyed by config id), so there
is no staleness/correctness reason to disable it and it is safe under concurrent
swarm access. Local stays cache-off **on purpose**: caching a local-disk repo
into a second local dir is pure write amplification for zero latency gain.

### Invariant
- `RusticVariant::Local` → `no_cache(true)` (deliberate, do not "fix").
- `RusticVariant::S3` → cache enabled with `cache_dir` under the per-pillbox
  state dir (scoped + cleanable), never rustic's global XDG dir.
- The cache never caches the scrypt-derived key — only index/pack data.
