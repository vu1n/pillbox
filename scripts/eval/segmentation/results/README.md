# σ̂-segmentation results

Durable trial records from the segmentation harness (`../run.sh`). Each file is
JSONL, one record per trial: `{task, cond, trial, score, cost}`. The verdict that
consumes them lives in [`docs/optimization-gate.md`](../../../../docs/optimization-gate.md)
(the σ̂-segmentation keystone section).

| file | run | n/arm | model | headline |
|---|---|---:|---|---|
| `ap_pov-glm51-n10.jsonl` | GHOST-007, 2026-06-14 | 10 | zai-coding-plan/glm-5.1 | σ̂ 0.467 → 0.000 (monolithic → segmented); mean 0.42 → 1.00 |
| `h1-3task-glm51-n10.jsonl` | H1 multi-task, 2026-06-15 | 10 | zai-coding-plan/glm-5.1 | 3 tasks (dot_dsl/grade_school/pov); pooled σ̂ 0.212 → 0.026; paired lift +0.41, CI [0.25, 0.53] excludes zero |
| `h2-segretries0-glm51-n10.jsonl` | H2 retry-isolation, 2026-06-15 | 10 | zai-coding-plan/glm-5.1 | same 3 tasks, `SEG_RETRIES=0`; σ̂ 0.251 → 0.037, lift +0.25 [0.13, 0.33] still excludes zero → retry not the driver |
| `h4-horizon-reset-glm51-n10.jsonl` | H4 reset-vs-scope, 2026-06-15 | 10 | zai-coding-plan/glm-5.1 | 3 arms (monolithic/chained/segmented); σ̂ 0.198/0.055/0.052; scope alone +0.52 [0.23,0.69], horizon-reset-on-top −0.025 [−0.075,0.0] → **scope is the mechanism, session reset adds nothing** |
| `enum-control-3task-glm51-n10.jsonl` | enumerated-monolithic control (`ENUM_MONO=1`), 2026-06-19 | 10 | zai-coding-plan/glm-5.1 | 4 arms; σ̂ mono 0.305 / enum 0.109 / chained 0.045 / seg 0.057; prompt-decomp (enum−mono) +0.17, Δσ̂ −0.196; checkpoint-gating (chained−enum) +0.18, Δσ̂ −0.064, pass 11→19/30 → **gating is a real, separable lever — "just a better prompt" refuted** |
| `control-serial-glm52-pov-n10.jsonl` | H5-inversion refutation, **serial** driver, 2026-06-19 | 10 | zai-coding-plan/glm-5.2 | 3 arms, ap_pov; σ̂ mono 0.292 / chained 0.000 / seg 0.000; mean 0.13 → 1.00. **Refutes the H5 "inversion"**: run serially, glm-5.2 chained σ̂ = 0.000 (== H4) — the cross-model campaign's chained σ̂ 0.317 was a **parallel-driver artifact** ($1.92) |
| `h5-opencode-go_deepseek-v4-pro-n10.jsonl` | H5 cross-model ⚠️ parallel driver, 2026-06-19 | 10 | deepseek-v4-pro (opencode-go) | 3 tasks × 3 arms; mono 0.50/σ̂0.29 → seg 0.91/σ̂0.15; mean lift **+0.41** ($10.32) |
| `h5-opencode-go_kimi-k2.7-code-n10.jsonl` | H5 cross-model ⚠️ parallel driver, 2026-06-19 | 10 | kimi-k2.7-code (opencode-go) | 3 tasks × 3 arms; mono 0.80/σ̂0.28 → seg 0.90/σ̂0.17; mean lift **+0.11** (high mono baseline) ($13.79) |
| `h5-zai-coding-plan_glm-5.2-n10.jsonl` | H5 cross-model ⚠️ parallel driver, 2026-06-19 | 10 | zai-coding-plan/glm-5.2 | 3 tasks × 3 arms; mono 0.47/σ̂0.38 → seg 0.88/σ̂0.19; mean lift **+0.41** ($7.12) |

> ⚠️ **The `h5-*` cross-model files came from the 5-models-concurrent driver.** The
> concurrency inflated σ̂ via VM boot/drive contention, so treat their per-model **σ̂
> as an upper bound** — the clean σ̂ is the serial `control-serial-*` run (chained
> 0.000). The cross-model **mean lift is robust** to contention and holds across all
> three models (mono → seg: +0.41 / +0.11 / +0.41). Run σ̂ campaigns **serially**.

Re-derive the stats from any file:

```sh
python3 ../../paired-stats.py --baseline monolithic --treatment segmented ap_pov-glm51-n10.jsonl
```

Reproduce a run: see the "Reproduce" block in the verdict doc. Run from an **immune
binary copy** — an external `cargo build` can clobber `target/debug` without the
libkrun feature → silent docker fallback → cost-0 / no-workspace zeros. And **reap
VMs between large batches**: a 30-session libkrun run can exhaust fresh-VM boot
capacity until resources are reclaimed (observed in the GHOST-007 run; the completed
batch was unaffected — verified by zero cost-0 records).
