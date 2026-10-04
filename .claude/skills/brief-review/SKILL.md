---
name: brief-review
description: Independent (L1) verification of a brief governance change — audit a `conforms` claim or an amendment proposal with a fresh-context agent. Use as the maintainer before ratifying an amendment, or to spot-check that code claiming to conform to a decision actually does. The adversarial verifier layer above the mechanical gate.
disable-model-invocation: true
---

# brief L1 review (independent verification)

The L0 gate is mechanical: it ensures decisions aren't edited by the coding loop and that
sign-offs are present. It cannot judge *substance* — whether code claiming to `conform`
actually does, or whether an amendment deserves ratification. L1 is an **independent,
fresh-context audit**: a verifier that did NOT make the change.

## When to run

- Before ratifying an amendment in `.brief/amendments/`.
- To sample `conforms` sign-offs — does the code actually satisfy the decision?

## How

Spawn a fresh agent (a subagent, or Codex via its rescue skill) — *not* the one that made
the change — and give it: the decision (`brief resolve <ref>`), the diff (`git diff`), and
the sign-off or amendment. Ask it to be skeptical:

**Auditing a `conforms` claim:**
> Does this diff actually satisfy the decision's invariant, or does it violate it while
> claiming conformance?
> VERDICT = CONFORMS | VIOLATES — plus 1–2 sentences.

**Auditing an amendment proposal:**
> Is this a legitimate evolution of the decision, or a routine task overturning a firm
> decision by fiat? Should it require human ratification?
> VERDICT = LEGITIMATE | REVERSAL_BY_FIAT, ESCALATE = yes/no, WHY (terse).

Run several independent verifiers and take majority when the call is close.

## Output

Record the verdict with the change (PR comment, or appended to the amendment file). A
`VIOLATES` or `REVERSAL_BY_FIAT` verdict blocks merge until the code is fixed or a human
ratifies (L3). **L1 advises; it never auto-ratifies** — only a human turns an amendment into
a new `active` decision.
