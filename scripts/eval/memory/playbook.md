# Playbook — memory bullets (the run-time-loop treatment)

Prepended to the task prompt in the `memory` condition. These are distilled from
the agent's OWN baseline failures (the hand-rolled ACE move — memory comes from
what the agent got wrong, not the answer key), kept general rather than
task-specific to avoid teaching-to-the-test.

Distilled 2026-06-02 from the GLM-4.5-air baseline sweep (0/6 on Aider-polyglot;
beer_song failed on output *structure*, not logic):

- **Match the exact output structure the spec asks for, not just the content.**
  When the result is a sequence (e.g. song verses, steps), check the granularity:
  usually each *line* is its own list element — do NOT join lines into one
  multi-line string. If verses/blocks repeat, they're typically separated by an
  empty-string (`""`) element. Re-read the instructions for the precise shape.
- **Nail the special cases exactly as worded** — singular vs plural ("1 bottle"
  vs "2 bottles"), and the zero/empty case (its wording is usually different and
  often wraps around). These are where format tasks silently fail.
- **Return the type the spec implies** (list vs string vs generator); don't
  print — return.
