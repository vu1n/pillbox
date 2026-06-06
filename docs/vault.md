# Credential vault (`--vault`)

> **Note (2026-06-01):** this describes the shipped stub-swap MITM. The
> [libkrun pivot](./libkrun-sandbox.md) hardens it to **vault v2**: the real key
> is substituted only on a **TLS handshake verified to an allowlisted host**
> (binds the credential to the destination), with **default-deny egress**, living
> in the guest's userspace egress stack instead of a per-container sidecar.

The vault keeps real Anthropic OAuth tokens on the host while the
sandboxed claude sees stubs. A pillbox-managed MITM HTTPS proxy swaps
stub → real on outbound requests to `api.anthropic.com` and
`console.anthropic.com`, and swaps real → stub on inbound responses
(so rotated tokens never reach the guest).

**v0.6 scope:** vault state is **per-pillbox**, and the CA is **per-run by
default** — each `--vault` run mints an ephemeral CA in a tempdir and discards it
after (blast radius = one run). An *opt-in stable* CA persists at
`<state_dir>/vault/` if you run `pillbox vault ca` (e.g. to pre-trust it); when
present, runs reuse it. A run inside project `myapp` would persist a stable CA at
`~/.pillbox/projects/<key>/vault/pillbox-vault-ca.crt`; `--pillbox global` uses
`~/.pillbox/global/vault/`. Leases never collide across pillboxes. See
[§Broker model](#broker-model-v2--the-policy-bound-egress-broker).

> **Egress note:** by default the proxy MITMs only matched hosts
> (Anthropic/OpenAI/GitHub) and **passes non-matched hosts through unmodified** —
> so out of the box it is not a general exfiltration guard. The fix is the
> **default-deny broker** ([§Broker model](#broker-model-v2--the-policy-bound-egress-broker)):
> the decision layer is built (`src/vault/egress.rs`, off by default), with CLI
> enablement + the backend egress fence as the next slices. See also
> [security.md](./security.md). The vault runs **on the host** today:
> the MITM proxy is an in-process server bound to a localhost port that
> the sandbox reaches over the loopback bridge. Because it lives in the
> CLI process, a vaulted run must stay in the foreground —
> `--detach` + `--vault` is unsupported (the proxy can't outlive the
> CLI).
>
> **Consequence for the swarm-memory scrub** ([swarm-memory.md](./swarm-memory.md)):
> that pipeline exact-matches outbound content against the vault's *real* secret
> values to strip them before pooling — but that is **zero-false-negative only
> for secrets sent to known-provider hosts, and only once strict-deny lands**. A
> secret exfiltrated to an *unmatched* host never transits an inspected path, so
> the scrub never sees it. Strict-deny egress is therefore a **prerequisite for
> cross-user pooling**, not just hardening.

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

# Run the configured agent with vault on
pillbox run --vault

# Force a specific agent regardless of pillbox.toml
pillbox run --agent codex --vault
```

That's it. The first `--vault` run generates the CA at
`<pillbox>/vault/pillbox-vault-ca.crt` and the matching private key,
leases a per-sandbox stub pair, starts an in-process MITM proxy bound
to a random localhost port, and wires `HTTPS_PROXY` +
`NODE_EXTRA_CA_CERTS` into the container.

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

- **Agent OAuth** (`pillbox run --vault`): proxy provisions a stub credentials FILE bind-mounted over the agent's real auth file. One lease per `--vault` run.
- **Secret API key** (`pillbox secret add NAME --vault` then `pillbox run --with NAME`): proxy provisions a stub VALUE injected as an env var in place of the real secret. One lease per vaulted `--with` per run.

Both kinds coexist in the same run — e.g. `pillbox run --vault --with ANTHROPIC_API_KEY` runs the agent with vaulted OAuth tokens AND a vaulted API key on the same `api.anthropic.com` host. AnthropicProvider branches by header (`Authorization: Bearer` → OAuth, `x-api-key` → API key).

Running `--vault` for an agent that isn't `vault_capable` errors with exit 2.

## Broker model (v2 — the policy-bound egress broker)

> **Status:** the decision core + CLI are **built** — `pillbox run --vault
> --egress-deny [--egress-allow HOST]…` enforces default-deny at the proxy
> (`src/vault/egress.rs`; off unless `--egress-deny`). The **libkrun backend
> fence** is already sole-egress, and the **SSRF/DNS-rebind guard** on its MITM
> forward leg is built. Remaining: the **docker** container-network fence + its
> forward-leg SSRF guard (hudsucker owns the dial), and a **per-run CA**. Design
> validated by a deep-research pass (`wmh2zb1y4`) — see
> [§Prior art](#prior-art--adopt-vs-build).

Today's vault is *stub-swap-on-known-host*: it MITMs provider hosts and **passes
everything else through**. That swaps credentials safely but is **not an
exfiltration guard** — a compromised agent can POST your code to `evil.example`
and the proxy waves it by. The broker reframe (per a review): the real
credential is released **only when session-identity + declared-secret +
destination-host + protocol all match policy**, and *unmatched egress doesn't
leave at all*.

### The decision — one chokepoint, three outcomes

Every outbound request resolves to exactly one of (`src/vault/egress.rs`,
`EgressPolicy::decide`):

| Decision | When | What happens |
|---|---|---|
| **Swap** | a provider intercepts the host | MITM + stub→real swap, **bound to that host** (the only path that releases a real secret) |
| **AllowPassthrough** | host on the explicit allowlist, *or* permissive mode (legacy) | tunnel unmodified, no MITM |
| **Deny** | default-deny on + no provider + not allowlisted | **blocked** — the request never leaves; agent gets a 403 |

`should_intercept` MITMs only Swap/Deny hosts (allowed hosts are tunnelled, *not*
MITM-everything); `handle_request` returns the 403 on Deny.

### The pieces (keep / refine / build)

- **Default-deny egress** — *the real security line.* Off by default today; when
  on, unmatched + un-allowlisted egress is denied. **Defense-in-depth, not a
  complete control**: SNI/Host filtering is bypassable (IP-literal, DoH, ECH
  blinds SNI, domain-fronting), so it must be paired with the backend egress
  fence (below) and DNS/IP controls. **Built — `--vault --egress-deny`.**
- **Destination-bound release** — a stub is only swapped on the host it's bound
  to (a provider intercepts only its own host(s); a leased `--with` secret
  records its `vault.host`). A stub replayed on `evil.example` is never swapped
  *and* (under default-deny) blocked. **Already true; default-deny closes the
  exfil channel.**
- **Network-layer enforcement (two modes)** — (a) **explicit-proxy** (`HTTPS_PROXY`
  + injected CA) for proxy-honoring clients (claude/codex/node) — *shipped*; (b)
  for clients that ignore proxy env, the security comes from the **backend egress
  fence set to sole-egress = the broker**. On **libkrun this is already the model**
  (`src/sandbox/libkrun/egress.rs`): the DNS fence NXDOMAINs every non-allowlisted
  name, allowlisted names resolve only to the in-VMM MITM gateway, and a
  hardcoded-IP / forged-SNI dial fails the pin gate — so all egress is *forced*
  through the broker or fails closed. On **docker** only the proxy-level
  default-deny applies — and *by design, not as a TODO*: docker's network can't
  be cleanly egress-fenced (Docker Desktop runs containers in a LinuxKit VM with
  no reachable host iptables; `--internal` severs the proxy path; DNS-only is
  bypassable via IP-literals). So a proxy-ignoring/compromised agent on docker
  *can* still dial direct — the run warns, and **libkrun is the airtight vaulted
  backend** (it owns the egress leg). A transparent redirect is a convenience,
  not a requirement.
- **SSRF / DNS-rebind guard** — refuse to forward to a real-upstream IP in a
  private/loopback/link-local/CGNAT/ULA range (cloud metadata `169.254.169.254`,
  `10.0.0.0/8`, a LAN box, `::1`) — an allowlisted *name* that resolves inward.
  **Built on the libkrun MITM forward leg** (`is_denied_egress_ip`, unit-tested);
  the docker broker's forward leg is hudsucker's connector, so that one's the gap.
- **Per-run CA** — **Built (default).** `--vault` runs now mint a fresh CA in a
  tempdir per run and discard it after, so a leaked CA is valid only for that one
  run. The guest installs the cert per-boot regardless (`NODE_EXTRA_CA_CERTS` +
  the system store), so ephemeral costs nothing. A **stable** CA is opt-in
  (`pillbox vault ca`, e.g. to pre-trust in a browser): if one exists at
  `<pillbox>/vault/`, runs reuse it. `pillbox vault status` reports which mode is
  in effect. (libkrun's MITM CA still uses the per-pillbox dir — a follow-up.)
- **Capability stubs** — stubs are high-entropy and looked up server-side.
  Caveat-based tokens (macaroons/biscuit) would bind {destination,session,expiry}
  cryptographically, but since pillbox keeps **all release decisions host-side**
  (no offline delegation), opaque-hashed stubs + server-side checks suffice;
  caveats are deferred unless delegation is ever wanted.

### Threat model — robust to a *fully*-compromised agent

The boundary is only real if **network-enforced**, not just an in-band token
check: the agent holds stubs and (under default-deny + sole-egress-to-broker) has
no other way out; the host broker holds the real creds and the network. Known
gap (Pluto Security): a secret-injecting proxy still leaves env vars, mounted
files, and the system prompt visible *inside* the sandbox — the vault protects
*vault-managed* credentials on the wire, not everything in the box.

### Prior art — adopt vs build

- **Adopt the shape of [iron-proxy](https://github.com/ironsh/iron-proxy)** — the
  closest shipping system: stub→real swap + default-deny (403 on unmatched) +
  per-host rules + an SSRF/DNS-rebinding guard (refuses to dial an allowlisted
  host that resolves into a denied CIDR — *we should add this*). Confirm its
  license before reusing code vs. design.
- **Precedent: CyberArk Secretless Broker** (Apache-2.0) — "secret never reaches
  the workload." Protocol-specific connectors, so not a drop-in for
  destination-bound HTTPS release (that's our build).
- **SPIFFE/SPIRE** = identity only (no release decision); optional identity layer.

### Cheapest validating experiment

`pillbox run --vault --egress-deny` and assert: (1) a replayed stub on
`evil.example` is **not** swapped and the request is **denied**; (2) unmatched
egress returns 403; (3) the real provider call still succeeds. The
`EgressPolicy::decide` unit tests cover the classification; the live assertion is
the next slice.

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
  │ ~/.pillbox/global/auth/claude/ │ ──mount──▶   │ /home/pillbox/.claude/   │
  │  .claude/               │              │  (real creds.json file)  │
  │   .credentials.json     │              │                          │
  │   (real OAuth tokens)   │              │                          │
  └──────────────┬──────────┘              └──────────────────────────┘
                 │
                 ▼
  ┌─────────────────────────┐              ┌──────────────────────────┐
  │ vault Server            │              │ stub creds.json (tmp)    │
  │ + per-sandbox lease     │ ──mount──▶   │ overlaid via -v file:    │
  │ + stub JSON → tempfile  │              │  /home/pillbox/.claude/  │
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

Per-pillbox CA + key. Path depends on the resolved scope:

```
<pillbox_state_dir>/vault/
├── pillbox-vault-ca.crt    # 0644 — self-signed root, valid 5 years
└── pillbox-vault-ca.key    # 0600 — sensitive
```

Examples:
- Global pillbox: `~/.pillbox/global/vault/`
- Project pillbox: `~/.pillbox/projects/-Users-x-work-myapp/vault/`

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
# Forget the CA for the current pillbox (forces regeneration on the
# next vault run; old guests will fail to verify until they're issued
# a new cert chain).
rm -rf "$(pillbox vault status --json | jq -r .ca_dir)"
```

There's no `pillbox vault forget` subcommand because removing the CA
should be a conscious manual step.

## See also

- [secrets.md](./secrets.md) — `--with` mounts secrets; vault swaps OAuth tokens
- [security.md](./security.md) — full threat model
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
