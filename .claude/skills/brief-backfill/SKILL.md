---
name: brief-backfill
description: Reconstruct a brief decision layer for an existing codebase from its code, docs, and comments. Use when adopting brief in a repo that already has ADRs / design docs / rationale comments — it maps what exists, verifies each decision against current code, carries forward the correct ones, re-grounds the drifted ones, and discovers decisions that live only in code. Invoke for "backfill brief", "adopt brief here", "reconstruct decisions".
disable-model-invocation: true
---

# brief backfill — reconstruct the decision layer

Turn an existing repo into a governed one. This is **migration + drift-audit + discovery
in one pass** — not a blind regeneration. Most existing decision docs are probably right;
verify and carry them forward, and only rebuild what has actually drifted.

## 1. Map (mechanical)

```sh
brief backfill            # writes .brief/backfill/map.md
```

Read `map.md`. It gives you: decision-genre docs (adr/design/spec/contract) with their
status-field + git-tracked flags, other docs (plans/reviews/runbooks — NOT decisions),
decision signals in code (existing `doc://` refs + rationale comments), and the module tree.

## 2. Reconcile each existing decision doc — verify, don't assume

For every decision-genre doc, extract its load-bearing claims and **check them against
current code** (read/grep the relevant module). Classify and act:

- **Current** (claims match code) → carry forward: author `.brief/docs/<id>.md`, *preserve
  the existing prose* (it's correct), add frontmatter (`status: active`), a stable anchor,
  and `related_code` globs. Don't rewrite good content.
- **Stale** (decision holds, details drifted) → carry forward + fix the drifted detail;
  note what you corrected.
- **Superseded / Contradicts / Orphaned** → do NOT import as active. Author the *current*
  decision (from code reality) and set `supersedes:`; leave the old doc in place but mark it
  `status: superseded`. Never delete.

## 3. Discover decisions that live only in code

Rationale/context comments and existing refs with no governing doc are decisions nobody
wrote down. Author them as new `.brief/docs/` decisions, scoped to the code they govern.

## 4. Author rules

- Use the `brief-author-decision` skill's format (frontmatter, one anchor per decision,
  invariant stated as the *why*).
- **Genre split:** do NOT import plans, reviews, runbooks, or readmes as decisions — they
  are snapshots/guides. (Give plans/reviews a `Snapshot as of DATE` note instead.)
- Prefer *few load-bearing* decisions over many trivial ones. Scope `related_code` tightly.
- **Verify before you assert.** If you can't confirm a claim against code, mark the doc
  `status: draft` and flag it for human review — never invent a decision.
- Then `brief publish <id>` each, add `// Context: doc://…@latest#<anchor>` refs at the
  governing sites the map flagged, and `brief pin`.

## 5. Reconciliation report

End with a summary: **carried-forward**, **re-grounded** (+ the drift found),
**newly-discovered** (decisions only in code), **superseded/archived**, and **needs human
review** (drafts you couldn't verify). This report *is* a docs-health audit — surface it.
