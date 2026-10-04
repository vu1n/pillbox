---
name: brief-amend
description: Propose an amendment when a coding task conflicts with a ratified (read-only) brief decision. Use when the gate blocks you with "ratified decision is read-only" or "amendment-required", or when you realize a task cannot be done without changing an active decision. The correct escalation — never edit the decision to make your code pass.
disable-model-invocation: true
---

# Proposing a brief amendment

A ratified (`status: active`) decision is READ-ONLY to coding work. If your task cannot be
done without changing it, you do **not** edit the decision and you do **not** force the code
through. You propose an amendment and STOP for human ratification. (Rewriting the decision to
ratify your own code is the one failure this system exists to prevent.)

## When this applies

- The gate blocked your commit with a `ratified-edit` or `amendment-required` message, or
- You can see your task contradicts an active decision's invariant before you start.

## Procedure

1. **Do not edit the decision doc**, and `git restore` any conflicting code you wrote. You
   cannot make a change valid by rewriting the constraint it violates.
2. Write `.brief/amendments/<anchor-id>.md`:

   ```md
   # Amendment proposal: <anchor-id>

   **Decision:** doc://<project>/<doc-id>@latest#<anchor-id>
   **Proposed by:** <agent/run>   **Date:** <date>

   ## What should change
   <the new invariant, or the clause to retire — precisely.>

   ## Why
   <the engineering reason the standing decision no longer holds, or the new requirement.>

   ## Code that needs it
   <the change blocked by the current decision; files / sketch.>

   ## Impact / risk
   <what else the decision protects; what breaks if it changes; migration notes.>
   ```
3. Record `<anchor-id> amend-proposed: <one-line reason>` in `.brief/SIGNOFF`.
4. Commit ONLY the amendment + SIGNOFF (no code). Never `--no-verify`.
5. **Stop and report** the task is blocked pending ratification. Do not keep trying to land
   the code, and do not look for a workaround.

A human ratifies the amendment (optionally advised by the `brief-review` skill); only then
does the decision move to a new revision and the code land.

**After ratification, run `brief doctor`.** The new revision changes the anchor's hash, so
every code ref pinned to the *old* revision is now `stale-ref` — the code was written
against a decision that has since moved. doctor lists each one; re-verify the code still
conforms and re-pin (`brief pin`). This is the drift a ratify silently creates, caught
mechanically instead of by a human noticing.
