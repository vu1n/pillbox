# pillbox docs

Topic-organized deep dives. For the agent-facing command reference, see
[../AGENTS.md](../AGENTS.md) at the repo root.

| File | When to read |
|---|---|
| [secrets.md](./secrets.md) | Storing API keys + `.env` bundles, lifecycle, precedence rules |
| [config.md](./config.md) | Per-project `pillbox.toml` — schema, discovery, merge rules |
| [strict.md](./strict.md) | `--strict` Gondolin microVM mode (flag ships v0.4, impl v0.5) |
| [recipes.md](./recipes.md) | Copy-paste flows for common tasks |
| [security.md](./security.md) | Threat model, file layout, what pillbox does and doesn't defend against |

If you're a coding agent dropping into a new machine, start with
`pillbox doctor --json` and read [AGENTS.md](../AGENTS.md).
