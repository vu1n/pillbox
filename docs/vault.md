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
```

That's it. The first `--vault` run generates `~/.pillbox/vault/pillbox-vault-ca.crt`
and the matching private key, leases a per-sandbox stub pair, starts an
in-process MITM proxy bound to a random localhost port, and wires
`HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` into the container.

The CA persists across runs — sandboxes share the trust root.

## What's in scope for v0.4

| Surface | Status |
|---|---|
| `claude` agent, OAuth tokens (`claudeAiOauth` block) | ✅ Vaulted |
| `api.anthropic.com` request bodies / headers | ✅ Stub → real swap |
| `console.anthropic.com/oauth/token` (rotation) | ✅ Real → stub swap inbound |
| `codex` agent | ❌ Not yet — defer to v0.5 |
| Anthropic API keys (`x-api-key` header via `--with`) | ❌ Not yet — OAuth path only |
| GitHub PATs | ❌ Not yet — defer to v0.5 |
| Other hosts (GitHub, OpenAI, etc.) | Pass through unmodified |

Running `pillbox claude run --vault` for a non-claude agent errors with
exit 2 (usage error).

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

Stubs encode the sandbox id (`sk-ant-oat01-<sandbox_id_compact><random>`)
so the proxy can resolve them without binding to TCP source-port. A
sandbox whose lease was dropped no longer resolves — re-using its stub
from outside gets 401.

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
- [strict.md](./strict.md) — `--strict` Gondolin microVM mode; the
  vault + strict interaction story lives there for now
- [security.md](./security.md) — full threat model
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
