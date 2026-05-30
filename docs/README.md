# pillbox docs

Topic-organized deep dives. For the agent-facing command reference, see
[../AGENTS.md](../AGENTS.md) at the repo root.

## Reference (shipped behavior)

| File | When to read |
|---|---|
| [secrets.md](./secrets.md) | Storing API keys + `.env` bundles, lifecycle, precedence rules |
| [config.md](./config.md) | Per-project `pillbox.toml` — schema, discovery, merge rules |
| [vault.md](./vault.md) | `--vault` MITM proxy that swaps stub creds for real ones |
| [observability.md](./observability.md) | OTLP telemetry — pointing pillbox at Workshop or any collector |
| [remotes.md](./remotes.md) | Remote backends (`ssh://`, `e2b://`) + detached sessions (shipped v0.6; successor designed in `remotes-redesign.md`) |
| [runner-image.md](./runner-image.md) | What the sandbox image contains + how it's built/published |
| [shared-mcp.md](./shared-mcp.md) | `--mcp` shared MCP attachments |
| [recipes.md](./recipes.md) | Copy-paste flows for common tasks |
| [security.md](./security.md) | Threat model, file layout, what pillbox does and doesn't defend against |

## Design / vNext (proposed — not yet shipped)

Start with the umbrella; the rest are deep specs it indexes.

| File | What |
|---|---|
| [vnext.md](./vnext.md) | **Start here.** The vNext umbrella — strategy, the layering, the build/defer/cut verdict, and the unified sequence |
| [session-event-log.md](./session-event-log.md) | Keystone (layer 1): the durable, attributed per-session event log |
| [gateway.md](./gateway.md) | The per-session sequencer + broker + attach endpoint §0 gates on (the no-daemon reconciliation) |
| [remotes-redesign.md](./remotes-redesign.md) | Backend collapse onto Docker contexts; BYO free / managed paid |
| [dx.md](./dx.md) | The developer-experience contract — the three inner loops + the zero-config-local principle |
| [swarm-memory.md](./swarm-memory.md) | Optimization/memory loops (external consumer): GEPA + ACE swarm memory over MCP, the privacy gate |

## Substrate specs (the contracts the vNext design builds on)

| File | What |
|---|---|
| [agent-io-contract.md](./agent-io-contract.md) | The PTY-free structured I/O contract (`agent.proto`); extended by `session-event-log.md` |
| [attach-transport.md](./attach-transport.md) | The interactive PTY / `Frame` transport; the cross-backend interface multiplayer + remotes ride on |

Archived decision records live in [`archive/`](./archive/).

If you're a coding agent dropping into a new machine, start with
`pillbox doctor --json` and read [AGENTS.md](../AGENTS.md).
