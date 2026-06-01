# pillbox docs

Topic-organized deep dives. For the agent-facing command reference, see
[../AGENTS.md](../AGENTS.md). **This index is the map — it marks what's
authoritative.** When a design doc and a banner disagree, the banner (newer) wins.

## Direction (read these first — current strategy + substrate)

| File | What |
|---|---|
| [vnext.md](./vnext.md) | **Start here.** The umbrella: strategy, layering, the unified sequence. (Has a 2026-06-01 direction banner: local-first; remote→Cloudflare/local-on-box; Docker→libkrun.) |
| [libkrun-sandbox.md](./libkrun-sandbox.md) | **The substrate.** Docker → libkrun microVM: own-via-FFI, `pillbox-init`, vsock control + smoltcp egress, the security union, what survives the pivot. |
| [dx.md](./dx.md) | The developer-experience contract — the inner loops + zero-config-local. (Remote-parity sections are superseded by the local-first pivot.) |

## Substrate / §0 spine (the contracts everything rides on)

| File | What |
|---|---|
| [session-event-log.md](./session-event-log.md) | The durable, attributed per-session event log — the keystone every consumer reads. |
| [gateway.md](./gateway.md) | The per-session sequencer + broker + attach endpoint §0 gates on. |
| [agent-io-contract.md](./agent-io-contract.md) | The PTY-free structured I/O contract (`agent.proto`). |
| [attach-transport.md](./attach-transport.md) | The interactive `Frame` transport. (Transport-agnostic surface; the `docker exec` carrier moves to vsock under libkrun.) |

## Reference (shipped behavior)

| File | When to read |
|---|---|
| [secrets.md](./secrets.md) | Storing API keys + `.env` bundles, lifecycle, precedence |
| [config.md](./config.md) | Per-project `pillbox.toml` — schema, discovery, merge |
| [vault.md](./vault.md) | `--vault` MITM proxy (stub-swap). *Hardening → vault v2 in libkrun-sandbox.md.* |
| [observability.md](./observability.md) | OTLP telemetry — pointing pillbox at a collector |
| [runner-image.md](./runner-image.md) | The sandbox image. *Docker framing; becomes a microVM rootfs under libkrun.* |
| [shared-mcp.md](./shared-mcp.md) | `--mcp` shared MCP attachments |
| [recipes.md](./recipes.md) | Copy-paste flows |
| [security.md](./security.md) | Threat model + file layout. *VM-boundary upgrade tracked in libkrun-sandbox.md.* |

## In progress

| File | What |
|---|---|
| [opencode-integration.md](./opencode-integration.md) | opencode as a server-mode run target (drive + read over its HTTP API). Wired + live-verified; transport moves to vsock under libkrun. pi is backlogged. |

## External consumer (separate project, not this repo)

| File | What |
|---|---|
| [swarm-memory.md](./swarm-memory.md) | Optimization / memory loops (GEPA + ACE) that consume the pillbox contract. |

## ⚠️ Superseded / deprecated (kept for history; do not build against)

| File | Status |
|---|---|
| [remotes-redesign.md](./archive/remotes-redesign.md) | **Superseded** by libkrun-sandbox.md (the Docker-context backend collapse is retired). Fork-from-store reasoning carries forward. |
| [remotes.md](./archive/remotes.md) | **Deprecated direction** — `ssh://`/`e2b://`/`docker://` still ship but are on the way out (remote → Cloudflare/local-on-box). |

Archived decision records: [`archive/`](./archive/).

If you're a coding agent on a fresh machine, start with `pillbox doctor --json`
and [AGENTS.md](../AGENTS.md).
