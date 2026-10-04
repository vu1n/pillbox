# Amendment proposal: managed-tier-do-gateway

**Decision:** doc://pillbox/managed-tier-do-gateway@latest#managed-tier-do-gateway

**Proposed by:** Claude (brief v0.4.2 upgrade)

**Date:** 2026-10-04

## What should change

Add `signoff: required` to this decision's frontmatter. Nothing else changes.

## Why

Since brief v0.3.0, sign-off is opt-in per decision. This one guards money: Pillbox
managed is a single-controller runtime and must not add a custom Durable Object,
remote §0 log, or per-delta storage. An agent adding "just a small DO" for logs or
session state reintroduces exactly the read/write cost the decision exists to prevent,
and the bill shows up long after review. A System One ranking of all 23 active pillbox
decisions (Jev, 2026-10-04) put this one fifth, at p=0.70, alongside the four security
boundaries.

Keeping the ask here means any change to that code gets an explicit "still conforms"
line that a reviewer reads, instead of passing silently.
