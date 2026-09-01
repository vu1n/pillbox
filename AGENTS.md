# pillbox — agent guide

This file is for **coding agents** (Claude Code, Codex, opencode, etc.)
using pillbox on a user's behalf. It explains the mental model in one
screen and documents every command an agent might need to run.

If you're a human, the README is friendlier. If you're an agent, this is
what you want.

> **⚠️ Direction note — ONE backend, a local microVM. Docker is DEPRECATED.**
> The backend is **a single local microVM**. **libkrun** is the current impl (the
> default build; macOS/HVF only today). **QEMU is under evaluation as the
> cross-platform single backend** (Mac+Linux+CI via HVF/KVM/TCG) and may replace
> libkrun — the microVM model and the vault/egress/§0 plumbing are shared, only the
> VMM launcher differs. **Docker is deprecated and on the path to deletion** — NOT
> a co-equal backend, NOT held to parity, and it gets **no new features and no
> bug-fix engineering**. Do not propose, build, or "also do" a docker version of
> anything. Do not defend docker with "it's cheap" / "cross-platform" / "the
> CF-container twin" — that framing repeatedly wasted the maintainer's time and is
> banned. A docker-only bug's resolution is "docker is deprecated," not a fix.
>
> **Core swarm primitives belong on the microVM backend.** pillbox exists for an
> independent orchestrator to spawn **swarms** — so `sandbox spawn/exec/agent` and
> `session send` are CORE, not optional. Anything that currently lives only on
> docker is a **bug to fix by porting to the microVM backend** (top of the
> backlog), never a reason to keep docker alive.
>
> The **remote** backend plane was removed (`remote add/list/info/rm`,
> `pillbox run --remote`, the `ssh://`/`e2b://`/`docker://` URL backends are gone).
> "Remote" returns as the managed/Cloudflare tier — a different shape, built
> fresh against CF's API, **not** a port of local docker (so docker earns no
> "twin" credit). Managed Pillbox is a **single-controller execution runtime**:
> Cloudflare's Sandbox Durable Object may own the container lifecycle, while
> bounded invocation claims live in D1, immutable evidence lives in R2, and
> collaboration belongs to Huddles. Pillbox must not introduce a custom Durable
> Object, remote §0 log, driver arbitration, replay broker, or per-delta storage.
> See `docs/managed-tier.md` and `docs/durable-object-usage.md`.
> Everything else (run, secrets, env, auth, vault, sessions, snapshots — and
> local detach/reattach) is current.

---

## Mental model — one concept

**A pillbox is a self-contained bundle of (workspace + code + vault +
config).** Users create pillboxes, then run agents against them.

| | What it is | Where it lives |
|---|---|---|
| **global pillbox** | One per OS user. Shared agent auth + fallback secrets/env. | `~/.pillbox/global/` |
| **project pillbox** | One per directory with `pillbox.toml`. Overrides global. | `~/.pillbox/projects/<dash-encoded-cwd>/` |
| **pillbox.toml** | Marks a directory as a project pillbox. Required field: `name`. | `./pillbox.toml` (walks up from cwd) |

Top-level commands act on **pillbox lifecycle** (init/new/list/rm/info).
Per-pillbox commands act on the **current** pillbox (run/secret/env/auth/
vault/...). The current pillbox is resolved by walking up from cwd
looking for `pillbox.toml`. No descriptor found → global. The `--pillbox
NAME` flag overrides discovery to point at a specific named pillbox.

---

## Quick start

```sh
# Bootstrap (one time)
pillbox init                                # creates ~/.pillbox/global/
pillbox auth login --agent claude           # OAuth in a sandbox

# Create a project pillbox
cd ~/work/myapp
pillbox new --name myapp                    # writes pillbox.toml + state

# Use it
pillbox secret add ANTHROPIC_API_KEY        # paste, then Ctrl-D
pillbox run                                 # mounts cwd at /workspace/myapp
```

Stop reading if that's all you need.

---

## Where to look — the map

Read the canonical doc for a subsystem **before** changing it or claiming how it
works. This map exists so you *retrieve* the right context instead of answering
from a partial memory of a large project.

| You need… | Go to |
|---|---|
| Full CLI / command + flag reference | [docs/commands.md](./docs/commands.md) |
| A load-bearing decision ("didn't we decide X?") | [docs/decisions.md](./docs/decisions.md) |
| System map + the entanglements that bite | [docs/architecture.md](./docs/architecture.md) |
| Backend / microVM substrate | [docs/substrate-plane.md](./docs/substrate-plane.md), [docs/libkrun-sandbox.md](./docs/libkrun-sandbox.md) |
| Vault (creds + egress) | [docs/vault.md](./docs/vault.md) |
| Sessions / §0 event log | [docs/session-event-log.md](./docs/session-event-log.md) |
| Dispatch / eval | [docs/dispatch.md](./docs/dispatch.md), [docs/eval.md](./docs/eval.md) |

## How to work here

- **Retrieve before asserting.** Before stating how a subsystem works or changing
  it, read its canonical doc (map above) **and** the relevant code. The project is
  larger than one context window; guessing yields confident wrong answers.
- **Code wins over docs.** If a doc disagrees with the code, the code is right —
  fix the doc in the same change. A doc you can't trust is worse than none.
- **Decisions are logged, not re-litigated.** [docs/decisions.md](./docs/decisions.md)
  is the record. Changing one = a new dated entry there, never a quiet reversal in
  chat or a contradicting edit elsewhere.
- **Backend = libkrun** (the direction note above). Docker's *backend* is
  deprecated/slated for deletion — don't add features to it.
- **Durable Objects are default-deny.** Cloudflare Sandbox may use its own DO as
  substrate; new Pillbox-authored DO classes require a ratified Brief amendment,
  a per-operation read/write budget, retention bound, kill switch, and cost
  telemetry before code. Never persist tokens, deltas, PTY frames, progress,
  logs, or replay rows in a DO. Use D1 for bounded relational claims and R2 for
  immutable/raw evidence. Run `node --test cloudflare-spike/do_usage_policy.test.mjs`
  when changing Cloudflare topology.

## Context Vault (brief)

Architectural decisions live in `.brief/docs/` as governance docs. Each has a stable
anchor (`<!-- brief:anchor id -->`), a `status`, and the code it governs (`related_code`
globs). A decision is addressable as `doc://<project>/<doc-id>@latest#<anchor>` — resolve
one with `brief resolve <ref>` to read the exact decision; don't infer it from the ref.

Decisions are **ratified constraints, not editable notes**. Develop *to* them:

- A decision with `status: active` is READ-ONLY to coding work. Do NOT edit it to make
  your change fit — your code conforms to the decision, not the reverse.
- When you change code a decision governs and it STILL satisfies that decision, record
  `<anchor-id> conforms: <reason>` in `.brief/SIGNOFF`.
- If the task CANNOT be done without changing a ratified decision, you may NOT change it
  yourself. Write `.brief/amendments/<anchor-id>.md` (what should change and why), record
  `<anchor-id> amend-proposed: <reason>` in `.brief/SIGNOFF`, and STOP — it needs human
  ratification before code can land. Never bypass the commit hook.
- Prefer a short `// Context: doc://...#anchor` ref over re-explaining a decision in code.

**Before committing, run `brief check`** (resolve anything it flags) and `brief pin`
(freeze any `@latest`/`@current` refs you wrote to a concrete revision). CI runs the same
check on PRs — that is the backstop; don't bypass it.
