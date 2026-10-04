---
name: brief-author-decision
description: Write or scope a brief governance decision doc in .brief/docs/. Use when capturing an architectural decision or invariant as a governed constraint — defining its anchor, the code it governs, and the invariant code must conform to. Invoke for "add a decision", "write an ADR in brief", "govern this invariant".
disable-model-invocation: true
---

# Authoring a brief decision

A decision doc records *why* — a constraint code must conform to — not *what* the code
does. One decision per file, one anchor per decision.

## File: `.brief/docs/<doc-id>.md`

```md
---
id: <doc-id>            # kebab, stable; e.g. adr-007-session-ownership
project: <project>
type: decision
status: active          # draft = freely editable while forming; active/ratified = LOCKED
title: <one line>
related_code:           # the governed surface — globs, not every file
  - "src/session/**"
  - "src/gateway.rs"
---

<!-- brief:anchor <anchor-id> -->
## <imperative statement of the decision>

<1–3 sentences: the decision / invariant.>

**Why.** <the reason — the durable value; code can't reconstruct intent cheaply.>

### Invariant
- <a checkable statement code must satisfy>
- <prefer invariants a test could later enforce (L2)>
```

## Rules

- **The anchor is a promise.** kebab-case, stable, never renamed after code references it.
  It is the citable API — pick it deliberately.
- **Scope `related_code` to the governed surface**, not the whole repo. Over-scoping trips
  the gate on every commit; under-scoping leaves the decision unenforced. Glob the files
  that actually embody the decision.
- **Write the invariant to be conformance-checkable.** "Every event carries an actor" beats
  "actors matter." A good invariant can later get a `test://` link (L2 enforcement).
- **Firmness:** start `draft` while forming (freely editable); promote to `active` once the
  team commits — then it is read-only to coding work and only an amendment can change it.
- **One decision per doc.** Overlapping `related_code` across decisions is fine and expected.

## After authoring

Reference it from each governing site with a one-line comment:
`// Context: doc://<project>/<doc-id>@latest#<anchor> — <the rule, in one line>`. The ref is
checked by the gate; the one-line rule is what the next agent actually reads at that spot,
and it outweighs stale memory there. Keep the reasoning in the doc, not the comment.

Then **run `brief doctor`** and close what it reports for this decision — it is how you
confirm you actually wired it, not just wrote it:

- `active-unwired` → you set `status: active` but no governed file references it. Add the
  `// Context:` ref.
- `active-unpublished` → an active decision with no frozen revision; `@latest` still points
  at a mutable doc. Run `brief publish <doc-id>`, then `brief pin`.
- `draft-governing-code` → code already leans on this decision while it's still `draft`
  (informational). When it firms up, promote + publish + pin.

`doctor` is advisory (won't block), but leaving its findings for a human review to catch is
the failure mode this closes.
