# Shared MCP attachments (`--mcp`)

**Status:** v0 — supported for `claude`, `codex`, and `opencode`
(each agent has its own injection adapter).

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
                                NAME: identifier shown to the agent
                                      ([A-Za-z][A-Za-z0-9_-]*).
                                URL: must be HTTP/HTTPS. host.docker.internal
                                is substituted for localhost/127.0.0.1.

--mcp-token NAME=SECRET_NAME    Repeatable. Attaches a bearer token to a
                                --mcp NAME=URL. SECRET_NAME refers to a
                                value stored via `pillbox secret add`.
                                Token values never appear in argv or
                                shell history; see "Token handling" below.
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

| Agent  | URL mechanism                                          | Token mechanism (`--mcp-token`)                                                                |
|--------|--------------------------------------------------------|------------------------------------------------------------------------------------------------|
| Claude | tempfile + `--mcp-config /etc/pillbox/mcp.json`        | Folded into the same 0600 tempfile JSON as `headers.Authorization: "Bearer <value>"`.          |
| Codex  | `-c mcp_servers.NAME.url="URL"` per attachment        | Env-var indirection: `PILLBOX_MCP_TOKEN_<NAME>=<value>` set via `-e`, referenced via `-c mcp_servers.NAME.bearer_token_env_var=…`. Token value never lands in argv. |

The codex path uses env-var indirection (vs inline `http_headers.Authorization` in argv) so `ps` on the host can't see the token. Symmetric with Claude's "token-in-0600-tempfile, not-in-argv" stance.

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

## Gotcha: DNS-rebinding protection on the server

Most MCP server libraries (FastMCP, Starlette/uvicorn-based servers,
many Express-based ones) ship with DNS-rebinding protection that
allows the `Host` header to be `localhost` or `127.0.0.1` *only*.
Pillbox rewrites the URL to `host.docker.internal` so the sandbox
can reach the host, which means the request arrives with
`Host: host.docker.internal:<port>` and the server rejects it.

The symptom on the Claude side is `/mcp` showing the server as
**failed** with a body like:

```
Streamable HTTP error: Error POSTing to endpoint: Invalid Host header
```

Pillbox can't fix this from the client side — the `Host` header is
derived from the URL by spec and no widely-used HTTP client honors a
user-set override. **The fix lives in the MCP server's config.** A
few common recipes:

| Server stack                 | Fix                                                                                  |
|------------------------------|--------------------------------------------------------------------------------------|
| FastMCP (Python)             | `FastMCP(..., transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False))` — or pass `allowed_hosts=["host.docker.internal", "localhost", "127.0.0.1"]` to scope it |
| Starlette / uvicorn (direct) | Drop `TrustedHostMiddleware`, or add `host.docker.internal` to its `allowed_hosts`   |
| Express + helmet             | Remove `helmet.hostCheck()` or extend its allowlist                                  |
| Custom Node/Go/Rust          | Whatever your `Host`-header allowlist is, add `host.docker.internal` to it           |

If the server you're attaching is third-party and you can't change
its config, the workaround is to run a thin reverse proxy (caddy,
nginx, socat) on the host that rewrites the `Host` header before
forwarding. Out of scope for pillbox.

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
- **Auth-granularity gap (for shared swarm memory)** — `--mcp-token`
  attaches **one bearer per attachment**, so every agent sharing one
  memory server shares one identity: no per-actor write attribution,
  no read-scoping. A shared swarm-memory server (see
  [swarm-memory.md](./swarm-memory.md)) that pools across users needs
  per-session/per-actor scoped tokens minted on the launch path — a
  prerequisite pillbox-side change. Single-tenant only until then.

## Token handling

`--mcp-token NAME=SECRET_NAME` sources the value from the same
secret store as `--with`. Tokens are designed to stay off the host
process listing:

- **Claude**: token folded into the 0600 tempfile JSON
  (`headers.Authorization: "Bearer <value>"`). Argv is token-free.
- **Codex**: token lands in the container env as
  `PILLBOX_MCP_TOKEN_<NAME>=<value>` (set via docker `-e`). The
  argv carries only the env var *name* via
  `-c mcp_servers.NAME.bearer_token_env_var=…`.

Codex's NAME → env var transform is uppercase + `-`→`_`:
`code-search` → `PILLBOX_MCP_TOKEN_CODE_SEARCH`. The resolver
detects and rejects collisions (e.g. `code-search` and
`code_search` collapsing to the same env var) with a clear error.

If `--mcp-token` is passed without a matching `--mcp NAME=URL`,
or the referenced secret is missing, pillbox errors at the CLI
boundary before launching the sandbox.

## v0 scope

- HTTP-transport MCP only
- Both agents wired: Claude (file-based) + Codex (`-c` flag)
- `--mcp NAME=URL` flag, repeatable. NAME is `[A-Za-z][A-Za-z0-9_-]*`
- `--mcp-token NAME=SECRET_NAME` for bearer auth, secret-store sourced
- `localhost` / `127.0.0.0/8` / `::1` / `*.localhost` → `host.docker.internal`
- Additive with persistent agent config (no `--strict-mcp-config`)
- `--mcp` + `--remote` rejected with a helpful error
- No supervision, no manifests, no scope registry, no lookup keys

## Not v0

- `--strict-mcp-config` mode for Claude reproducibility
- `--remote` support (remote-side attachment + URL reachability
  is its own design)
- Stdio MCP (escape hatch: provider author ships an HTTP wrapper)
- Auto-discovery / persistent `pillbox.toml` MCP declarations
- Vault stub-swap for `--mcp-token` (third-party MCP tokens
  aren't worth proxying through the Anthropic-shaped vault)
