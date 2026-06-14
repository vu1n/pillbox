# σ̂-segmentation results

Durable trial records from the segmentation harness (`../run.sh`). Each file is
JSONL, one record per trial: `{task, cond, trial, score, cost}`. The verdict that
consumes them lives in [`docs/optimization-gate.md`](../../../../docs/optimization-gate.md)
(the σ̂-segmentation keystone section).

| file | run | n/arm | model | headline |
|---|---|---:|---|---|
| `ap_pov-glm51-n10.jsonl` | GHOST-007, 2026-06-14 | 10 | zai-coding-plan/glm-5.1 | σ̂ 0.467 → 0.000 (monolithic → segmented); mean 0.42 → 1.00 |

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
