# Shared MCP attachments (`--mcp`)

**Status:** v0 — Claude only. Codex injection is a documented
follow-up.

Pillbox can point a sandboxed agent at one or more HTTP MCP
servers running on the host. The "shared" property falls out for
free: nothing stops two sandboxes from being attached to the same
URL — they hit the same warm process, see the same state, no
extra plumbing needed.

Pillbox has **no opinion** on what runs at the other end of the
URL — supervision, lifecycle, schema, authz are all the user's
(or orchestrator's) problem. Pillbox's job is the wiring.

## Quick start

```sh
# Start an MCP server somewhere on the host
docker run -d -p 7777:7777 mem0ai/openmemory:latest

# Attach it to a sandboxed run
pillbox run --mcp openmemory=http://localhost:7777

# Attach multiple
pillbox run --mcp openmemory=http://localhost:7777 \
            --mcp canopy=http://localhost:7000
```

The flag value is `NAME=URL`. `NAME` is what the agent sees in
its MCP tool list; `URL` is the host-side endpoint (pillbox rewrites
`localhost` / `127.0.0.1` to `host.docker.internal` before injection
so the sandbox can reach it).

## CLI surface

```
--mcp NAME=URL                  Repeatable. Adds a shared MCP server.
                                NAME: identifier shown to the agent.
                                URL: must be HTTP/HTTPS. host.docker.internal
                                is substituted for localhost/127.0.0.1.
```

Combinations:

- `--mcp` is **additive** with the user's persistent MCP config
  (`~/.claude.json` etc) — per-run servers extend, they don't
  replace. The persistent home is bind-mounted; pillbox doesn't
  touch it.
- `--mcp` + `--remote` → **rejected**. Remote sandboxes can't see
  a host-local MCP URL on the caller's machine. v1 follow-up may
  add remote-side attachment.

## How it's wired

For each `--mcp NAME=URL`:

1. Pillbox generates a per-run MCP config file in a tempdir on
   the host (cleaned up at run exit via `tempfile::NamedTempFile`,
   same pattern as the vault's stub creds).
2. The tempfile is bind-mounted read-only into the sandbox at a
   fixed guest path (e.g. `/etc/pillbox/mcp.json`).
3. The agent's argv is extended with the appropriate
   per-invocation flag so the agent loads it without mutating its
   persistent home.

Per-agent details:

| Agent  | Flag                                | Notes                                  |
|--------|-------------------------------------|----------------------------------------|
| Claude | `--mcp-config /etc/pillbox/mcp.json` | Additive with persistent config. Use `--strict-mcp-config` later if reproducibility demands it. |
| Codex  | _not yet wired_                     | Planned: `CODEX_HOME=<tmpdir>` overlay with seeded `config.toml`. |

The injection contract lives in the agent adapter (`src/agents/`),
not in shared run code — each agent has different per-run config
mechanics.

## URL rewriting

The agent runs inside Docker. The host's `localhost` is not the
sandbox's `localhost`. Pillbox rewrites:

- `http://localhost:N/...`   → `http://host.docker.internal:N/...`
- `http://127.0.0.1:N/...`   → `http://host.docker.internal:N/...`

Anything else (a real hostname, an IP, a DNS-resolvable name) passes
through unchanged. The sandbox already gets `--add-host=
host.docker.internal:host-gateway` for the vault, so the alias
resolves on Linux too.

## The cross-pillbox channel

Shared MCP = cross-sandbox channel by construction. Anything a
provider lets one agent write, another agent can read on the next
attachment. Mitigations are operational, not technical:

- **Per-`pillbox run` opt-in** — `--mcp` is never default. The
  user (or orchestrator) explicitly asks for each attachment on
  each invocation.
- **Provider responsibility** — namespacing / auth / capability
  splits live in the MCP server, not in pillbox. Pillbox does not
  pretend to add a security layer it cannot enforce on a process
  it didn't start.

## v0 scope

- HTTP-transport MCP only
- Claude only (Codex deferred)
- `--mcp NAME=URL` flag, repeatable
- `localhost` → `host.docker.internal` rewrite
- Additive with persistent agent config (no `--strict-mcp-config`)
- `--mcp` + `--remote` rejected with a helpful error
- No supervision, no manifests, no scope registry, no lookup keys

## Not v0

- Codex injection (different config-file mechanics; needs its
  own pass)
- Per-attachment bearer tokens (mem0 OpenMemory local doesn't
  need them; add when a real consumer does)
- `--mcp NAME=URL --mcp-token NAME=SECRET_NAME` for stored-secret
  bearer auth
- `--strict-mcp-config` mode for reproducibility
- `--remote` support (remote-side attachment + URL reachability
  is its own design)
- Stdio MCP (escape hatch: provider author ships an HTTP wrapper)
- Auto-discovery / persistent `pillbox.toml` MCP declarations
