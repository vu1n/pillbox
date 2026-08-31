<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/pillbox-logo-dark.png">
    <img alt="pillbox" src="assets/brand/pillbox-logo-light.png" width="420">
  </picture>
</p>

<p align="center"><b>Durable coding-agent sessions on Cloudflare. Sovereign local microVMs when the work belongs on your machine.</b></p>

<p align="center">
  <a href="#cloudflare-is-the-headliner">Cloudflare</a> &nbsp;·&nbsp;
  <a href="#local-is-the-superpower">Local</a> &nbsp;·&nbsp;
  <a href="#ghost-the-proof-tenant">Ghost</a> &nbsp;·&nbsp;
  <a href="docs/commands.md">Commands</a> &nbsp;·&nbsp;
  <a href="SECURITY.md">Security</a>
</p>

<p align="center">
  <a href="https://github.com/vu1n/pillbox/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/vu1n/pillbox/actions/workflows/ci.yml/badge.svg"></a>
</p>

---

An agent process is disposable. Its session should not be.

Pillbox gives a coding agent one durable, ordered event stream that people,
agents, IDEs, and orchestrators can replay, drive, annotate, and hand off. We
call that stream **§0**: the session's source of truth for messages, tool calls,
inputs, actors, checkpoints, and externally verified results.

The runtime has two placements:

- **Cloudflare managed (experimental):** a Worker runs one bounded turn in
  Cloudflare Sandbox. D1 stores the claim and terminal state, R2 stores one
  immutable evidence artifact, and the caller appends that evidence to its
  local §0 log. Huddles owns multiplayer collaboration.
- **Local (working alpha):** a libkrun microVM runs the agent on your machine,
  with a local §0 log, credential broker, encrypted snapshots, detach/reattach,
  and verified workspace handoffs.

Ghost is the conductor layer we are dogfooding on top: snapshot a real tree,
delegate bounded work to isolated microVM workers, judge results with executable
checks, and bring one passing workspace back for review.

> **Project status:** pre-alpha. The local path is real and used for Ghost. The
> managed foreground path is implemented and has been live-validated, but it is
> not yet a hosted product or a polished end-user service. The gaps are listed
> openly below.

## One runtime contract, two placements

Local Pillbox runs the agent in a libkrun microVM and keeps the session log on
disk. Experimental managed Pillbox runs one bounded turn in Cloudflare Sandbox,
stores the invocation claim in D1 and immutable evidence in R2, then appends the
returned evidence to the caller's local log.

Cloudflare Sandbox's vendor-owned Durable Object may own container lifecycle.
Pillbox does not add a custom DO, remote event sequencer, replay broker, driver
lease, or participant roster. Huddles owns those multiplayer concerns.

Every terminal run records raw usage units—model tokens and provider cost,
D1/R2 operations and bytes, Analytics Engine points, Sandbox time/profile—in a
versioned cost envelope. Inspect it with `pillbox session cost <id>`.

The managed tier remains experimental: foreground OpenCode turns are the
implemented capability; managed detach/reconnect, hosted token UX, and a stable
public deployment remain open. See the [managed runtime](./docs/managed-tier.md)
and [Durable Object policy](./docs/durable-object-usage.md).
[`cloudflare-spike/README.md`](./cloudflare-spike/README.md).

## Local is the superpower

The local path is not a lesser offline mode. It is where pillbox can make strong
promises that a hosted sandbox cannot:

- **Sovereign execution.** The agent runs in a hardware-isolated libkrun
  microVM on your machine.
- **Credentials it can use but not read.** The host-owned egress broker swaps
  stub credentials at the network boundary and can default-deny unmatched
  destinations.
- **Real handoffs.** Detach from a terminal, reattach elsewhere, drive a session
  with `session send`, watch or subscribe to its durable log, and annotate it
  without taking the driver role.
- **Portable workspaces.** Encrypted, content-addressed rustic snapshots give
  local and managed runs one handle space.
- **Verified delegation.** Fork workers from an exact snapshot, grade their
  work with executable checks, and pull one coherent winner instead of merging
  fragments from multiple agents.

### Install and run locally

The supported alpha path today is macOS/HVF. A Docker daemon is temporarily
needed to pull and unpack the OCI runner image; the agent itself runs in the
libkrun microVM, not in the Docker backend. The Docker agent backend is
deprecated and receives no new features.

```sh
# libkrun runtime
brew tap libkrun/krun
brew trust libkrun/krun
brew install libkrun libkrunfw

# build, install, and sign the local binary
git clone https://github.com/vu1n/pillbox.git
cd pillbox
./scripts/install.sh

# one-time setup
pillbox doctor
pillbox init
pillbox auth login --agent claude

# create a project pillbox and run
cd ~/work/my-project
pillbox new --name my-project
pillbox run
```

Pillbox also integrates Codex, opencode, and pi. See the [command
reference](./docs/commands.md) and [local microVM
architecture](./docs/libkrun-sandbox.md) for current prerequisites and agent
profiles.

### Hand a live session to another terminal or actor

```sh
SESSION="$(pillbox run --agent claude --detach --label handoff --json \
  | jq -r '.session.id')"

# another terminal or an orchestrator can observe and steer it
pillbox session watch "$SESSION"
pillbox session send "$SESSION" $'Check the failing integration test.\n'

# someone else can chime in without driving
pillbox session annotate "$SESSION" \
  "The regression started in the cache key change." --anchor src/cache.rs

# a human can take the terminal again; Ctrl-A D detaches without killing it
pillbox session attach "$SESSION"
```

For machine consumers, `pillbox session subscribe "$SESSION" --from 1` serves
the same replay-then-tail log over WebSocket.

## Ghost: the proof tenant

Ghost is the conductor gesture being derived through real use, not a finished
standalone product. Today the shipped loop is `pillbox dispatch`:

1. snapshot the current workspace, including intentional WIP;
2. fork one or more isolated libkrun workers from that exact snapshot;
3. drive a bounded task;
4. grade each result with an external command or rubric;
5. select a passing worker and pull one coherent workspace back for review;
6. retain the verdict and local §0 evidence.

```sh
pillbox push --bookmark ghost-demo

# Start with k=1 until the task has a real diversity hypothesis.
# A passing winner is pulled into the current workspace, so review the diff.
pillbox dispatch --from-bookmark ghost-demo -k 1 \
  --agent opencode \
  --cmd "cargo test --all-targets" \
  -- "Fix the failing parser tests without changing the public contract."
```

The reproducible wiring demo lives in
[`scripts/smoke/dispatch.sh`](./scripts/smoke/dispatch.sh), and the full verdict
contract is in [`docs/dispatch.md`](./docs/dispatch.md).

What Ghost is **not** claiming yet: a shipped `ghost run` CLI, Cloudflare-backed
execution, a general DAG planner, critic/beam search, guaranteed best-of-k gains,
or fully parallel turns. Ghost remains an in-repo experimental tenant until its
conductor contract stabilizes.

## Security boundary

Pillbox is designed to keep host credentials and unrelated host state outside
the agent sandbox. On the local path, real credentials remain at the host-owned
egress boundary; the guest receives stubs. Workspace snapshots exclude
pillbox-controlled secret patterns and are encrypted before reaching R2.

This is pre-alpha security software, not a claim of perfect isolation. Read the
[threat model](./docs/security.md), [vault design](./docs/vault.md), and
[security policy](./SECURITY.md) before using pillbox with sensitive work.

## Where to look

- [`docs/architecture.md`](./docs/architecture.md) — verified system map and
  structural debts.
- [`docs/session-event-log.md`](./docs/session-event-log.md) — the §0 contract.
- [`docs/managed-tier.md`](./docs/managed-tier.md) — Cloudflare architecture,
  implementation status, and open questions.
- [`cloudflare-spike/`](./cloudflare-spike/) — bounded D1/R2 execution and
  Cloudflare Sandbox path.
- [`docs/dispatch.md`](./docs/dispatch.md) — verified worker-loop contract.
- [`scripts/ghost/`](./scripts/ghost/) — Ghost research tenant and evidence.
- [`AGENTS.md`](./AGENTS.md) — coding-agent guide and canonical-doc router.

## Contributing

Pillbox is early enough that a clear falsifier or a small end-to-end proof is
more valuable than a broad feature proposal. Start with
[`CONTRIBUTING.md`](./CONTRIBUTING.md), then open an issue describing the
contract you want to exercise and how it will be verified.

## License

Licensed under either [Apache License 2.0](./LICENSE-APACHE) or
[MIT](./LICENSE-MIT), at your option.
