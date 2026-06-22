# Live smoke suite

The guard CI can't be. GitHub CI runs `cargo test` on macos+ubuntu — **unit only**:
no libkrun VM, no Docker, no live agent, no `wrangler dev`. So the paths that
actually break in practice (VM boot, agent drive, the §0 producers, `session
pull`, the CF §0 gateway) ship green until someone pokes them by hand. This suite
codifies that poking into one repeatable command — run it before merging anything
that touches the substrate. (It's the net that would have caught the opencode
bring-up regression which merged CI-green.)

## Run

```sh
scripts/smoke/run.sh            # libkrun (opencode) + CF — the reliable default
scripts/smoke/run.sh libkrun    # just the libkrun agent path
scripts/smoke/run.sh cf         # just the CF §0 gateway
SMOKE_CODEX=1 scripts/smoke/run.sh libkrun   # also smoke codex-serve (opt-in)
```

## What it checks

- **`scripts/lk-build.sh`** — build `--features libkrun` **and** codesign in one
  step (a bare `cargo build`/`test`/`clippy` strips the HVF signature → silent
  docker fallback). Run it before any manual libkrun run too.
- **`libkrun.sh <agent> <image> [model]`** — boot a libkrun server session, drive
  a mechanical edit, then assert: a §0 `usage` event flowed (producer works),
  agent output flowed (drain works), `session pull` recovered the edit (the live-
  workspace write-back), and the **original** workspace is untouched (fork-from-
  store). Tests the plumbing, not the model's cleverness — the edit is a verbatim
  string replace so the assertion is deterministic.
- **`cf.sh`** — `tsc` + `wrangler dev` + the auth / driver-arbitration /
  annotation smokes (`test-auth.mjs`, `smoke-actor.mjs`, `smoke-driver.mjs`).

## Prereqs

- macOS with the libkrun toolchain (for the libkrun smokes).
- Agents authed: `pillbox auth login --agent opencode` (and `--agent codex` for
  codex-serve); a reachable model for opencode (`SMOKE_MODEL`).
- Runner images present: `pillbox-runner:dev` (opencode), `pillbox-runner:dev`
  (codex-serve). Override via `OPENCODE_IMAGE`/`CODEX_IMAGE`.
- `cloudflare-spike/` deps installed (`npm i`) + Node ≥ 23 (for `.ts` imports) for
  the CF smokes.

codex-serve is opt-in (`SMOKE_CODEX=1`): its bring-up is more sensitive and it
currently needs the `l8` image, so it's kept out of the default gate to avoid
cry-wolf failures.
