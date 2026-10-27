---
id: project-setup
paths:
  - "src/pillbox.rs"
  - "src/config.rs"
  - "src/paths.rs"
  - "src/cli.rs"
  - "src/main.rs"
  - "src/secrets.rs"
  - "src/envs.rs"
  - "src/commands/secret.rs"
  - "src/commands/env.rs"
  - "src/commands/auth.rs"
  - "src/agents/mod.rs"
entry:
  - "src/pillbox.rs:discover_from"
  - "src/config.rs:resolve_run_config"
  - "src/paths.rs"
tests:
  - "tests/lifecycle.rs:discovery_walks_up_to_find_descriptor"
  - "tests/lifecycle.rs:secret_inheritance_global_to_project"
  - "src/config.rs:overlay_project_wins_global_fills_gaps"
---

# Project bundles and configuration

Users create named project bundles and inherit global defaults, secrets and agent authentication. The CLI selects the closest project descriptor or the global bundle. See [configuration](../config.md) and [commands](../commands.md).

## Sub-features

- Global initialization and project creation/list/info/removal.
- Upward descriptor discovery and explicit `--pillbox` selection.
- Field-by-field run defaults, named presets, secret/env inheritance and global agent auth.

## How to get to it (user POV)

Run `pillbox init`, then `pillbox new --name demo` in a disposable project. `pillbox info --json` reports the selected project; from a nested directory it still reports `demo`. `pillbox --pillbox global info --json` selects the global bundle.

## Driving it with the repository harness

`cargo test --no-default-features --test lifecycle` runs the real CLI in temporary homes and directories. Expect descriptor/state creation, nested discovery, explicit selection and secret inheritance assertions to pass. `cargo test --no-default-features config::tests` exercises default overlays and rejects malformed configuration without starting an agent.

## Gotchas

- Discovery selects one project; defaults overlay global fields rather than merging every ancestor descriptor.
- Unknown top-level config fields fail, while the workspace table is permissive.
- `auth login` still has Docker image/container plumbing; this map does not claim the auth flow was ported to libkrun.
