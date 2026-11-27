# Shared MCP attachments (`--mcp`)

**Status:** v0 — supported for `claude`, `codex`, and `opencode` on the local
libkrun microVM (each agent has its own injection adapter). Every other agent,
and the managed tier, refuses `--mcp`/`--mcp-token` with a usage error (exit 2)
rather than ignoring it.

Pillbox can point a sandboxed agent at one or more HTTP MCP
servers — on the host's loopback, or remote over HTTPS. The "shared" property falls out for
free: nothing stops two sandboxes from being attached to the same
URL — they hit the same warm process, see the same state, no
extra plumbing needed.

Pillbox has **no opinion** on what runs at the other end of the
URL — supervision, lifecycle, schema, authz are all the user's
(or orchestrator's) problem. Pillbox's job is the wiring.

## Quick start

```sh
# Start any HTTP MCP server on the host's loopback, e.g. on port 7777

# Attach it to a sandboxed run
pillbox run --mcp openmemory=http://localhost:7777

# Attach multiple
pillbox run --mcp openmemory=http://localhost:7777 \
            --mcp canopy=http://localhost:7000
```

The flag value is `NAME=URL`. `NAME` is what the agent sees in
its MCP tool list; `URL` is where the server is, as seen from the host. See
"Reaching the server from the microVM" for which URLs a run accepts.

A first-party example ships in the repo:
[`scripts/mcp/code-explore`](../scripts/mcp/code-explore/) — a read-only
repo-orientation server (`explore_code` / `find_pattern`, backed by `rg` +
`ast-grep`) that hands a sandboxed agent compact `file:line` citations. Serve it,
then `pillbox run --mcp code-explore=http://localhost:8123`.

## CLI surface

```
--mcp NAME=URL                  Repeatable. Adds a shared MCP server.
                                NAME: identifier shown to the agent
                                      ([A-Za-z][A-Za-z0-9_-]*).
                                URL: http(s):// on the host's loopback
                                (localhost / 127.0.0.1 / [::1]:PORT), or
                                https://<dns-name>[:443]/… for a remote
                                server.

--mcp-token NAME=SECRET_NAME    Repeatable. Attaches a bearer token to a
                                --mcp NAME=URL. SECRET_NAME refers to a
                                value stored via `pillbox secret add`.
                                Token values never appear in argv or
                                shell history; see "Token handling" below.
```

Combinations:

- `--mcp` is **additive** with the user's persistent MCP config
  (`~/.claude.json` etc) — per-run servers extend, they don't
  replace. Pillbox writes its own file next to the agent's config in
  the guest home clone and doesn't touch the persistent one.
- The managed tier refuses `--mcp` (URL reachability from a hosted
  sandbox is its own design).

## How it's wired

Each agent adapter (`src/agents/mcp.rs`) renders the attachments as a
substrate-neutral injection — guest files, extra agent argv, guest env — and the
libkrun launch lands it: files are written 0600 into the run's guest-home clone
(never the persistent auth home), argv is appended to the agent command, env
joins the guest env.

| Agent    | URL mechanism                                                        | Token mechanism (`--mcp-token`)                                                                |
|----------|----------------------------------------------------------------------|------------------------------------------------------------------------------------------------|
| Claude   | `~/.pillbox/mcp.json` + `--mcp-config <that path>`                   | Folded into the same 0600 file as `headers.Authorization: "Bearer <value>"`.                   |
| Codex    | `-c mcp_servers.NAME.url="URL"` per attachment                       | Env-var indirection: `PILLBOX_MCP_TOKEN_<NAME>=<value>` in the guest env, referenced via `-c mcp_servers.NAME.bearer_token_env_var=…`. Token value never lands in argv. |
| OpenCode | `~/.pillbox/opencode-mcp.json` named by `OPENCODE_CONFIG`            | Folded into that 0600 file as `headers.Authorization: "Bearer <value>"`.                       |

A server-mode agent (opencode serve) takes file + env injections only; an
adapter that needs argv is refused there.

## Reaching the server from the microVM

The guest's `localhost` is not the host's, and the microVM's only network path
is the egress gateway (`10.0.2.2`). Each URL is routed one of two ways; anything
else is a usage error:

- **Host loopback** — `http(s)://localhost|127.0.0.1|[::1]:PORT/…`. The gateway
  forwards `10.0.2.2:PORT` to the host's `127.0.0.1:PORT`, and the agent is
  given the URL rewritten to `10.0.2.2:PORT`. Ports 53 and 443 are refused: the
  gateway's own DNS and TLS listeners own them.
- **Remote** — `https://<dns-name>[:443]/…`. The host name joins the run's
  egress allowlist, so the fence resolves it and the gateway's TLS leg carries
  it. Plain `http://`, a port other than 443, an IP address, or another
  loopback name is refused: the fence resolves allowlisted names and
  terminates only 443.

## Gotcha: DNS-rebinding protection on the server

Most MCP server libraries (FastMCP, Starlette/uvicorn-based servers,
many Express-based ones) ship with DNS-rebinding protection that
allows the `Host` header to be `localhost` or `127.0.0.1` *only*.
A host-loopback attachment reaches the server through the gateway with
`Host: 10.0.2.2:<port>`, which such a server rejects.

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
| FastMCP (Python)             | `FastMCP(..., transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False))` — or pass `allowed_hosts=["10.0.2.2:7777", "localhost:7777", "127.0.0.1:7777"]` to scope it |
| Starlette / uvicorn (direct) | Drop `TrustedHostMiddleware`, or add `10.0.2.2` to its `allowed_hosts`               |
| Express + helmet             | Remove `helmet.hostCheck()` or extend its allowlist                                  |
| Custom Node/Go/Rust          | Whatever your `Host`-header allowlist is, add `10.0.2.2` to it                       |

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

- **Claude / OpenCode**: token folded into the 0600 guest config file
  (`headers.Authorization: "Bearer <value>"`). Argv is token-free.
- **Codex**: token lands in the guest env as
  `PILLBOX_MCP_TOKEN_<NAME>=<value>`. The argv carries only the env
  var *name* via `-c mcp_servers.NAME.bearer_token_env_var=…`.

Codex's NAME → env var transform is uppercase + `-`→`_`:
`code-search` → `PILLBOX_MCP_TOKEN_CODE_SEARCH`. The resolver
detects and rejects collisions (e.g. `code-search` and
`code_search` collapsing to the same env var) with a clear error.

If `--mcp-token` is passed without a matching `--mcp NAME=URL`,
or the referenced secret is missing, pillbox errors at the CLI
boundary before launching the sandbox.

## v0 scope

- HTTP-transport MCP only
- Three agents wired: Claude and OpenCode (file-based) + Codex (`-c` flag)
- `--mcp NAME=URL` flag, repeatable. NAME is `[A-Za-z][A-Za-z0-9_-]*`
- `--mcp-token NAME=SECRET_NAME` for bearer auth, secret-store sourced
- Host-loopback servers forwarded at the gateway; remote servers over
  `https://<dns-name>:443` only
- Additive with persistent agent config (no `--strict-mcp-config`)
- No supervision, no manifests, no scope registry, no lookup keys

## Not v0

- `--strict-mcp-config` mode for Claude reproducibility
- Managed-tier support (URL reachability from a hosted sandbox is
  its own design)
- Stdio MCP (escape hatch: provider author ships an HTTP wrapper)
- Auto-discovery / persistent `pillbox.toml` MCP declarations
- Vault stub-swap for `--mcp-token` (third-party MCP tokens
  aren't worth proxying through the Anthropic-shaped vault)
