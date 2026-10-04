# Amendment proposal: secret-on-disk-0600

**Decision:** doc://pillbox/secret-on-disk-0600@latest#secret-on-disk-0600

**Proposed by:** Claude (brief v0.4.2 upgrade)

**Date:** 2026-10-04

## What should change

Add `signoff: required` to this decision's frontmatter. Nothing else changes.

## Why

Since brief v0.3.0, sign-off is opt-in per decision: a decision without the field no
longer asks for a `conforms` line when a change touches the code under its
`// Context:` comment. That is right for most of pillbox's 23 decisions, but this one
guards a security boundary, where silent drift is costly and hard to notice:

Every secret file goes through the one 0600 helper. A new write path that bypasses it leaves creds, repo passwords or the CA key world-readable.

Keeping the ask here means any change to that code gets an explicit "still conforms"
line that a reviewer reads, instead of passing silently.


---
ratified_rev: 0002
ratified_by: Vu (approved in project chat 2026-10-04)
