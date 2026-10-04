# Amendment proposal: libkrun-env-fork-substrate

**Decision:** doc://pillbox/libkrun-env-fork-substrate@latest#libkrun-env-fork-substrate

**Proposed by:** Claude (brief v0.4.2 upgrade)

**Date:** 2026-10-04

## What should change

Add `signoff: required` to this decision's frontmatter. Nothing else changes.

## Why

Since brief v0.3.0, sign-off is opt-in per decision: a decision without the field no
longer asks for a `conforms` line when a change touches the code under its
`// Context:` comment. That is right for most of pillbox's 23 decisions, but this one
guards a security boundary, where silent drift is costly and hard to notice:

The guest must never hold the real credential; egress is MITM-swapped in a host-owned stack. A change that forwards the real secret into the guest breaks the core isolation promise.

Keeping the ask here means any change to that code gets an explicit "still conforms"
line that a reviewer reads, instead of passing silently.
