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
  - "src/sandbox/libkrun/login.rs"
entry:
  - "src/pillbox.rs:discover_from"
  - "src/config.rs:resolve_run_config"
  - "src/paths.rs"
tests:
  - "tests/lifecycle.rs:discovery_walks_up_to_find_descriptor"
  - "tests/lifecycle.rs:secret_inheritance_global_to_project"
  - "src/config.rs:overlay_project_wins_global_fills_gaps"
  - "src/sandbox/libkrun/login.rs:claude_login_vm_opens_token_endpoints_mounts_the_real_home_and_forwards_the_callback"
  - "src/sandbox/libkrun/login.rs:login_without_an_oauth_port_has_no_callback_forward"
  - "src/sandbox/libkrun/egress.rs:only_the_login_constructor_opens_token_endpoints_and_the_spec_file_keeps_it"
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

`auth login` needs a real HVF boot. On the Mac, stop Docker Desktop's daemon (the runner rootfs must already be cached under `~/.pillbox/krun/`) and run `pillbox auth login --agent claude`. A microVM boots and shows claude's login prompt; open the printed URL, finish in the browser (the callback reaches the guest through the relay), and expect `✓ claude authenticated` with `~/.pillbox/global/auth/claude/.claude/.credentials.json` written. `cargo test sandbox::libkrun::login` checks the login VM spec (token endpoints open, real home share, callback forward only with an OAuth port) without a boot.

## Gotchas

- Discovery selects one project; defaults overlay global fields rather than merging every ancestor descriptor.
- Unknown top-level config fields fail, while the workspace table is permissive.
- `auth login` runs in a foreground libkrun microVM, never Docker; a build without libkrun refuses it. The agent's real auth home (`~/.pillbox/global/auth/<agent>/`) is mounted read-write, without the env-fork or the ownership normalization runs apply to their clones, and the boot script lives in a separate scratch share, so nothing but the agent's own files lands there. It is the only launch built with `EgressSpec::for_login`, which lets the guest reach the providers' OAuth token endpoints; the `login` field is private to `egress.rs`, so every other launch uses `EgressSpec::fence` and can't open them. The login VM stays foreground (`run_foreground_attached`): a reparented VM's commit guard scrubs every share, and this share is the real auth home. A login that ends any way but the agent's own exit (a dropped attach, a detach) kills its VM before reaping it (`KillUnlessExited`), so it can't hang. codex gets `SSL_CERT_FILE` pointed at the vault CA so its Rust TLS stack trusts the MITM. An agent with an OAuth callback port (claude 54545, pi/prime-agent 53692, override with `PILLBOX_<AGENT>_OAUTH_PORT`) gets a host listener on `127.0.0.1:<port>`, relayed over vsock to the guest's `vsock-forward`, so a busy port fails the login before boot.
