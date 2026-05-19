# Credential vault (`--vault`)

The vault keeps real Anthropic OAuth tokens on the host while the
sandboxed claude sees stubs. A pillbox-managed MITM HTTPS proxy swaps
stub → real on outbound requests to `api.anthropic.com` and
`console.anthropic.com`, and swaps real → stub on inbound responses
(so rotated tokens never reach the guest).

For the command reference, see [../AGENTS.md](../AGENTS.md).

## When to use it

- You're running claude on untrusted code (PRs from strangers, malicious
  prompts, plugin code you didn't write) and want exfiltration of your
  OAuth token to be useless to an attacker.
- You want to be able to rotate tokens without touching the sandbox —
  pillbox handles the rotation transparently.

**Don't use it when:** you don't trust pillbox itself, or when you need
the agent to talk to non-Anthropic hosts that pillbox doesn't proxy
(those still pass through unmodified, but no token swap happens).

## Quick start

```sh
# Pre-generate the CA (optional — happens lazily on first --vault run)
pillbox vault ca

# Check vault state
pillbox vault status

# Run claude with vault on
pillbox claude run --vault

# Codex works the same way (v0.5).
pillbox codex run --vault
```

That's it. The first `--vault` run generates `~/.pillbox/vault/pillbox-vault-ca.crt`
and the matching private key, leases a per-sandbox stub pair, starts an
in-process MITM proxy bound to a random localhost port, and wires
`HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` into the container.

The CA persists across runs — sandboxes share the trust root.

## What's vaulted

| Surface | Status |
|---|---|
| `claude` agent, OAuth tokens (`claudeAiOauth` block) | ✅ |
| `api.anthropic.com` request bodies / headers | ✅ Stub → real swap |
| `console.anthropic.com/oauth/token` (rotation) | ✅ Real → stub swap inbound |
| `codex` agent, ChatGPT-mode OAuth tokens (`tokens` block) | ✅ (v0.5) |
| `chatgpt.com`, `chat.openai.com` request headers | ✅ Stub → real swap (v0.5) |
| `auth.openai.com/oauth/token` (rotation) | ✅ Real → stub swap inbound (v0.5) |
| `codex` ApiKey mode (`OPENAI_API_KEY` in auth.json) | ❌ — use `--with OPENAI_API_KEY` via the secret-vault path instead |
| Anthropic API keys (`x-api-key` header via `--with`) | ✅ (v0.5) — see [secrets.md](./secrets.md#vaulted-secrets) |
| OpenAI API keys (`Authorization: Bearer` to api.openai.com via `--with`) | ✅ (v0.5) |
| GitHub PATs (`Authorization: Bearer` / `token` to api.github.com via `--with`) | ✅ (v0.5) |
| All other hosts | Pass through unmodified |

Two vault flavors:

- **Agent OAuth** (`pillbox claude run --vault`, `pillbox codex run --vault`): proxy provisions a stub credentials FILE bind-mounted over the agent's real auth file. One lease per `--vault` run.
- **Secret API key** (`pillbox secret add NAME --vault` then `pillbox <agent> run --with NAME`): proxy provisions a stub VALUE injected as an env var in place of the real secret. One lease per vaulted `--with` per run.

Both kinds coexist in the same run — e.g. `pillbox claude run --vault --with ANTHROPIC_API_KEY` runs claude with vaulted OAuth tokens AND a vaulted API key on the same `api.anthropic.com` host. AnthropicProvider branches by header (`Authorization: Bearer` → OAuth, `x-api-key` → API key).

Running `--vault` for an agent that isn't `vault_capable` errors with exit 2.

## Architecture

The proxy holds a list of `VaultProvider`s. Each provider owns:

- the host predicate (`api.anthropic.com` for claude; `chatgpt.com` +
  `chat.openai.com` + `auth.openai.com` for codex)
- the credentials file path inside the guest (`.claude/.credentials.json`
  for claude; `.codex/auth.json` for codex)
- the stub format (`sk-ant-oat01-` / `sk-ant-ort01-` for claude;
  `pb-codex-oat-` / `pb-codex-ort-` for codex)
- the request/response swap logic

Adding a new vaulted service (e.g. GitHub PATs in a future PR) means
implementing the `VaultProvider` trait in a new file under
`src/vault/providers/` and registering it in `providers::registry()`.
Server core doesn't change.

A single shared `Registry` holds `sandbox_id → SandboxData` and
`stub_token → sandbox_id` lookups across all providers. Stubs never
collide because each provider has its own prefix.

## How it works

```
   host                                       guest (docker)
  ┌─────────────────────────┐              ┌──────────────────────────┐
  │ ~/.pillbox/data/claude/ │ ──mount──▶   │ /home/lum/.claude/       │
  │  .claude/               │              │  (real creds.json file)  │
  │   .credentials.json     │              │                          │
  │   (real OAuth tokens)   │              │                          │
  └──────────────┬──────────┘              └──────────────────────────┘
                 │
                 ▼
  ┌─────────────────────────┐              ┌──────────────────────────┐
  │ vault Server            │              │ stub creds.json (tmp)    │
  │ + per-sandbox lease     │ ──mount──▶   │ overlaid via -v file:    │
  │ + stub JSON → tempfile  │              │  /home/lum/.claude/      │
  └──────────────┬──────────┘              │  .credentials.json:ro    │
                 │                         └──────────────────────────┘
                 │
                 │                         ┌──────────────────────────┐
                 │                         │ HTTPS_PROXY=             │
                 │                         │  http://host.docker.     │
                 │                         │  internal:<port>         │
                 │                         │ NODE_EXTRA_CA_CERTS=     │
                 │                         │  /etc/pillbox-ca.crt     │
                 │                         └──────────────┬───────────┘
                 │                                        │
                 │  ◀──── HTTPS via proxy ─────────────────┘
                 │
                 ▼
  ┌─────────────────────────┐              ┌──────────────────────────┐
  │ MITM intercept on       │              │  Anthropic upstream      │
  │ api.anthropic.com /     │ ──TLS──▶     │  (real connection,       │
  │ console.anthropic.com:  │              │   real token only here)  │
  │  • stub → real outbound │              └──────────────────────────┘
  │  • real → stub inbound  │
  │ Everything else: pass-  │
  │ through (no MITM).      │
  └─────────────────────────┘
```

Stubs encode the sandbox id (e.g. claude:
`sk-ant-oat01-<sandbox_id_compact><random>`, codex:
`pb-codex-oat-<sandbox_id_compact><random>`) so the proxy can resolve
them without binding to TCP source-port. A sandbox whose lease was
dropped no longer resolves — re-using its stub from outside gets 401.

## Files on disk

```
~/.pillbox/vault/
├── pillbox-vault-ca.crt    # 0644 — self-signed root, valid 5 years
└── pillbox-vault-ca.key    # 0600 — sensitive
```

The CA cert is mounted read-only into the guest at
`/etc/pillbox-ca.crt`. The private key never leaves the host.

The per-run stub JSON lives in a tempfile that's deleted when the run
exits.

## Limits + caveats

- **macOS / Docker Desktop only (tested).** The proxy assumes
  `host.docker.internal` works for container→host networking. Linux
  needs the `--add-host=host.docker.internal:host-gateway` flag, which
  pillbox passes unconditionally — should work on recent Docker. Not
  smoke-tested on Linux.
- **MITM = trust pillbox.** The proxy holds your real OAuth tokens in
  memory while the container runs. If pillbox is compromised, those
  tokens are exposed.
- **Only Anthropic hosts are intercepted.** Other HTTPS traffic flows
  through unmodified (no certificate spoofing on those hosts).
- **Node.js path only.** `NODE_EXTRA_CA_CERTS` wires the CA into claude
  (a Node app). Non-Node agents would need a different trust strategy.

## Cleanup

```sh
# Forget the CA (forces regeneration on next vault run; old guests will
# fail to verify the proxy if reused without re-issued certs).
rm -rf ~/.pillbox/vault/
```

There's no `pillbox vault forget` subcommand because removing the CA
should be a conscious manual step.

## See also

- [secrets.md](./secrets.md) — `--with` mounts secrets; vault swaps OAuth tokens
- [security.md](./security.md) — full threat model
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
