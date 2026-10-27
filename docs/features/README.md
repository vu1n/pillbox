# Pillbox feature map

Start here to connect a behavior to its implementation, entry points and tests.
These maps describe the code; they do not approve decisions or impose a governance
process. Code wins when prose drifts. Read the linked subsystem docs alongside it.

## Baseline and driving conventions

Run commands from the repository root unless a map says otherwise. Python 3.9+
and Git run the map helper without dependencies. Rust integration tests use temporary
homes/projects through `tests/common/mod.rs`; they do not use your real agent login.
The toolchain-free Rust build uses `--no-default-features`; that is useful offline
coverage, not proof of the maintained libkrun backend. Feature-gated tests require
the libkrun libraries even when they do not boot a VM.

Live local smokes require macOS/HVF, libkrun/libkrunfw, a signed binary from
`scripts/lk-build.sh`, prepared runner images and agent auth. They consume model
turns and are separate from the offline checks. Managed tests use Node.js 22 as in
CI (a version supporting native TypeScript stripping), installed npm dependencies
and local workerd. Managed production burn-in and observed cost evidence are
separate release gates. Test references are existing evidence to inspect or run,
not a claim that every live path was re-driven while writing this map.

| Feature | What to find |
|---|---|
| [Project setup](project-setup.md) | Bundle discovery, config defaults, secrets/env and agent auth |
| [Local microVM](local-microvm.md) | Image/rootfs preparation, boot and control channels |
| [Vault and egress](vault-egress.md) | Host-owned credential refresh/substitution and outbound policy |
| [Sessions](sessions.md) | Registry, sequenced events, artifacts, send/read and attachment |
| [Workspace snapshots](workspace-snapshots.md) | Rustic stores, bookmarks, ingest and independent forks |
| [Session packets](session-packets.md) | Deterministic pack/unpack for engine callers |
| [Repository execution](repository-execution.md) | Exact committed input, bounded execution and verifier evidence |
| [Text execution](text-execution.md) | Tool-free harness turns and durable retry identity |
| [Dispatch](dispatch.md) | Protected grading, worker selection and segment chains |
| [Managed execution](managed-execution.md) | Experimental authorization, D1 claims, R2 evidence and compute |

This is an initial map of the main execution surfaces, not exhaustive coverage.
Sidecar collection, MCP attachments, observability exporters and the experimental
Ghost/eval research tooling merit more detailed maps when those areas change.
Shared code can touch several features; an unmapped path is a retrieval gap, not
permission to skip its existing tests. Design-only Ghost conductor plans are not
listed as shipped features.

## Keep the map useful

```sh
python3 scripts/featuremap.py features src/execution/store.rs
python3 scripts/featuremap.py features --base origin/main
python3 scripts/featuremap.py check
python3 -m unittest discover -s scripts -p 'test_featuremap.py' -v
```

Before editing, read the returned entry points, tests and Gotchas. Update the
relevant map in the same PR when behavior or pointers change, then drive that
feature. `--base` compares committed `BASE...HEAD`; pass filenames explicitly
for planned or uncommitted work. Glob checks use tracked files, so stage newly
added implementation files before checking their globs.

The checker rejects dead globs, missing files/symbols, duplicate IDs and missing
paths. It only notes missing tests and does not judge prose, completeness or
whether a referenced test proves the claimed behavior; reviewers still check
those. Keep 2–3 focused entry points, actual behavior tests, sub-features, user
flow, exact driving commands with observable results, and verified Gotchas.

## Helper provenance and retired history

`scripts/featuremap.py` is vendored unchanged from
[`vu1n/dev` at `19eee845fe2d5a9e6aee8d67ac2c3cfc993071a0`](https://github.com/vu1n/dev/blob/19eee845fe2d5a9e6aee8d67ac2c3cfc993071a0/skills/feature-map/scripts/featuremap.py).
The [canonical skill](https://github.com/vu1n/dev/blob/19eee845fe2d5a9e6aee8d67ac2c3cfc993071a0/skills/feature-map/SKILL.md)
defines the format. CI runs the local copy without fetching it at runtime.

Brief was removed in PR #206; this adds the feature maps deferred there.
[Archived decision records](../archive/brief/README.md) preserve the final historical
texts and their original statuses separately from these current behavior maps.
Drafts and the proposed session-packet entry remain unratified. Historical
`doc://` references and approval language are inert source material, not live
instructions or a revived Brief check.
