# Strict mode (`--strict`)

`pillbox <agent> run --strict` is the opt-in for hardware-isolated
sandboxing: instead of Docker (shared kernel), the agent runs inside a
Gondolin microVM (independent kernel, QEMU or krun).

## Status — v0.4

**The flag ships; the implementation does not.** Passing `--strict`
today returns:

```
pillbox: run failed. --strict (Gondolin microVM) is unavailable in this build
  Next: pillbox claude run   # use the default Docker sandbox
```

Exit code 2 (usage error).

The CLI shape is locked in v0.4 so scripts can be written ahead of
time. A matching `strict` field in `pillbox.toml` and the Gondolin
spawn integration both land in v0.5.

## Why ship the flag without the impl

Two reasons:

1. **Public surface stability.** Once `--strict` is part of `--help`, we
   can't move it. Locking it in v0.4 lets users and downstream tools
   write `pillbox claude run --strict` today without worrying about
   future renames.
2. **Roadmap honesty.** v0.4's README originally promised --strict.
   Shipping a working flag with a clear "not yet wired" error is more
   honest than silently dropping the promise.

## Why microVMs

Docker shares the host kernel. A container escape or kernel-level
exploit reaches the host. Gondolin runs each sandbox in its own VM with
its own kernel — a much bigger jump for an attacker.

The trade-off: VM boot is slower than Docker (~1-3s vs ~200ms cold) and
the disk image is larger. Most users won't need `--strict`; it's for
high-stakes runs where defense in depth matters.

## The v0.5 integration plan

Gondolin's main entrypoint is a TypeScript daemon
([github.com/earendil-works/gondolin](https://github.com/earendil-works/gondolin))
that supervises microVMs. There's also `gondolin-rs`, a Rust client lib
that speaks Gondolin's session IPC over Unix sockets — but `gondolin-rs`
only *connects* to running VMs, it doesn't spawn them.

v0.5 needs one of:

- **Embedded daemon**: pillbox bundles or starts the Gondolin daemon
  on-demand for `--strict` runs. Adds a Node.js dependency.
- **Out-of-process daemon**: user runs Gondolin separately; pillbox
  connects via `gondolin-rs::Session::find(uuid)`. Cleaner but worse
  UX (extra setup step).
- **Pure-Rust spawn**: extend `gondolin-rs` to spawn VMs directly
  (currently only Phase 1: client-side ops). Biggest upstream change.

Which path we take depends on how gondolin-rs evolves between now and v0.5.

## What `--strict` won't change

- Persistent agent HOME at `~/.pillbox/data/<agent>/` — same path, same
  perms, same content. The VM bind-mounts it the same way Docker does.
- Workspace mount at `/workspace/<name>` — same shape.
- Secret + env composition (`--with`, `--env`, `--env-file`) — same
  precedence rules.
- pillbox.toml resolution — same discovery.

What does change: the runtime image (Docker → VM disk image), the boot
time, and the network namespace.

## Interaction with `--vault`

Open question for v0.5. The vault proxy currently listens on the host
and the Docker container reaches it via `host.docker.internal`.
Gondolin VMs have their own network namespace and would need a
different routing strategy (e.g., a virtio-vsock channel).

Both flags are independent — `--vault --strict` will be valid once both
ship, but the wiring needs care.

## See also

- [security.md](./security.md) — full threat model and what each
  sandbox tier defends against
- [vault.md](./vault.md) — credential vault (orthogonal to `--strict`)
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
