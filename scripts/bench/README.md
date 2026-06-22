# Startup benchmark

`startup.py` runs pillbox startup cases, reads host-emitted
`session.started` lifecycle events, and summarizes `startup_ms` plus per-stage
timings.

## Examples

```sh
# One Docker PTY case, five measured runs after one warmup.
scripts/bench/startup.py --case docker-claude --warmup 1 --repeat 5

# Server-mode opencode on Docker and libkrun, with a fixed model.
# Defaults to the same opencode image used by the smoke/eval scripts:
# PILLBOX_RUNNER_IMAGE=pillbox-runner:dev.
scripts/bench/startup.py \
  --case docker-opencode \
  --case libkrun-opencode \
  --model zai-coding-plan/glm-4.5-air \
  --warmup 1 \
  --repeat 5

# Show the commands without starting agents.
scripts/bench/startup.py --all-docker --dry-run

# Emit raw measured runs plus aggregate summaries.
scripts/bench/startup.py --all-docker --json
```

## Notes

- Cases require the corresponding agent auth and backend support.
- opencode cases default to `pillbox-runner:dev`; `libkrun-codex-serve` defaults
  to `pillbox-runner:dev`, matching `scripts/smoke/`. Override with
  `--runner-image IMAGE` or `PILLBOX_RUNNER_IMAGE`.
- Every run uses `--detach --json --ttl` and is removed with
  `pillbox session rm` after its event is captured.
- By default, the benchmark creates one temporary empty workspace and reuses it
  for every run. Use `--workspace PATH` to benchmark a real tree.
- Commands time out after 300 seconds by default. Override with
  `--timeout SECONDS` for slow cold-start environments.
- Use `--json` for raw run records and aggregate summaries, or `--csv` for one
  row per measured startup stage.
- `PILLBOX` can point at a specific binary; otherwise the script uses
  `target/debug/pillbox` when present, then falls back to `pillbox` on `PATH`.
