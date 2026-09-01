# Vault OAuth refresh coordination — single-writer token rotation (design)

Status: **design / proposed** (2026-06-19). A credential-correctness design for
running one subscription OAuth account (Claude, Codex) across **multiple
concurrent pillbox sessions** — `dispatch -k`, concurrent `--detach` runs,
multiple projects — without tripping the provider's refresh-token reuse
detection and getting the whole session logged out.

Companion to [vault.md](./vault.md) (the broker mechanics), the
[managed-tier](./managed-tier.md) (the DO substrate this extends), and the
[security model](./security.md). Sibling concern to the substrate-ultra-review's
credential boundary.

---

## The one-line problem

> Anthropic's OAuth uses **strict refresh-token reuse detection**. If two clients
> hold the same refresh token and both rotate it, the side that rotates *second*
> is treated as a stolen credential and the **entire token family is revoked** —
> logging every session out at once and forcing a fresh browser login.

(This is documented externally — e.g. the centaur project's deploying-in-production
guide — but the mechanism is provider-side, so it binds anyone sharing one OAuth
account across clients.) API-key mode sidesteps it (no refresh token), but that's
a non-starter when the whole point is to use a flat-rate **subscription**.

pillbox is **structurally exposed** to this, more than a typical CLI:

1. **One global OAuth credential, shared by everything.** Auth is global-only
   (`~/.pillbox/global/auth/<agent>/`); every run, project, and session reads the
   same refresh-token family.
2. **`pillbox dispatch` forks `k` parallel workers on it** by design
   (`commands/dispatch.rs` — `workers: u32`, each a `run --from-bookmark --detach`).
   That's N concurrent clients on one token family.
3. **No cross-process refresh authority.** Each `pillbox run --vault` spins its
   *own* transient broker (`vault::Server::start` per invocation — no daemon), and
   nothing serializes rotation across them.

### Why the existing guard doesn't cover it

`vault/session.rs` already has a `is_at_least_as_fresh` check at teardown
(`Drop for VaultSession`) — explicitly commented as "the concurrent-session
clobber" guard. But it only orders the **on-disk write-back** (an older session
must not overwrite a fresher token already on disk). It does **nothing** to stop
two brokers from both sending the *same* refresh token to the provider's
`/oauth/token`. The reuse-revoke happens at the provider, before any disk write.
And the persist runs at *teardown* — far too late for an overlapping session.

Concrete failing timeline today:

```
Run A and Run B start concurrently, both read RT0 from the global creds file.
A's pre-refresh (session.rs → refresh::refresh_real_if_expired): RT0 -> RT1, persist RT1.
B's pre-refresh: still holds RT0 (read before A persisted) -> sends RT0 to provider.
Provider: "RT0 reused" -> revoke the whole family -> A and B both dead -> forced re-login.
```

---

## The invariant

```
The sandbox can USE credentials, but cannot READ them.   (env-fork: already true)
The broker can READ credentials, but releases them only to declared hosts.  (host-bound swap: #86/#87)
The refresh token has exactly ONE writer.                (this design)
```

The first two hold today. This document is about the third.

---

## What pillbox already has (the 80% that's built)

- **The vault is the single refresh interceptor.** On `--vault` the broker
  pre-refreshes an expired token before launch
  (`vault/session.rs::provision_oauth_mount` → `vault/refresh.rs::refresh_real_if_expired`,
  refreshing `PRE_EXPIRY_BUFFER` ahead of `expiresAt`), intercepts in-run
  `/oauth/token` rotations (`vault/providers/anthropic.rs::handle_oauth_request` /
  `handle_response`), and persists the rotated pair back to the global creds file.
- **The shared artifact already exists**: the global creds file — the agent's
  `cred_sentinel` under its auth home
  (`~/.pillbox/global/auth/claude/.claude/.credentials.json`,
  `~/.pillbox/global/auth/codex/.codex/auth.json`) — every run reads it.
- **`VaultProvider` trait** (`vault/providers/mod.rs`) already owns per-provider
  host predicates, guest creds paths, stub minting, and request/response swap.
- **Host-bound release** (#86/#87): a stub presented to the wrong provider host
  fails closed (no swap), closing cross-host replay.
- **The env-fork**: the guest mounts *stubs*; the real never enters the workload
  (and on **libkrun**, never enters the VM — the MITM lives in the reparented VMM
  child, not an `HTTPS_PROXY` the guest could bypass).

The missing piece is small and specific: **serialized, single-writer rotation**.

---

## The design — host-side single-writer refresh

### Correction to the obvious-but-wrong version

The intuitive fix is "put the token in a shared mount so all pillboxes see it."
**Do not put the real refresh token in a guest mount** — that re-exposes it to the
untrusted agent and defeats the env-fork. The shared artifact stays **host-side**
(the global creds file all brokers already read/write); the guests keep their
**stubs**; only the host-side vault processes ever touch the real. And a shared
file alone gives you reads, not *serialization* — that needs a lock.

### Primitive 1 — a locked `TokenStore` (the single writer)

A small host-side type wrapping the per-provider/profile creds file plus a lock:

File layout (the creds path is the agent's `cred_sentinel` *under* the auth home —
e.g. `~/.pillbox/global/auth/claude/.claude/.credentials.json`):

```
~/.pillbox/global/auth/claude/
  .claude/.credentials.json      # real tokens + rotation_generation + pending marker (ONE file, written atomically)
  pillbox-auth.lock              # flock target — the critical section
```

The `rotation_generation` and the `pending` marker live **inside the creds file**
(or a single sidecar written-and-renamed together with it), not a second
`meta.json` — two files can't be updated atomically, and a crash between them
desyncs the generation from the tokens.

```rust
struct TokenStore { creds_path: PathBuf, lock_path: PathBuf }

impl TokenStore {
    /// The ONLY path that mints/rotates a real token. Holds an exclusive
    /// cross-process flock (bounded wait), re-reads the CURRENT creds from disk
    /// inside the lock, and returns the live token to swap — never a per-run
    /// registry copy. The in-proxy `/oauth/token` handlers call this and
    /// synthesize the guest response from its result; they MUST NOT forward the
    /// guest's stub-mapped refresh token to the provider directly.
    fn refresh_if_needed(&self, provider: &dyn VaultProvider) -> Result<LiveTokens>;
}
```

#### The exact protocol (Primitive 1 hinges on this)

The naive "add a lock around the persist" is **not** correct: today the in-proxy
handlers resolve the guest's stub to the **registry's start-of-run real token**
(`anthropic.rs` / `codex.rs` read from `server.registry`), so two runs that both
launched with `RT0` will each forward `RT0` even with a lock around the disk write.
The lock must own the *whole* rotation, re-reading from disk, and a refresh token
must be **POSTed at most once, ever**:

```
refresh_if_needed (exclusive flock, bounded wait):
  re-read (access, refresh, expiresAt, generation, pending) FROM DISK   # never the registry
  if pending is set for the current generation:
      # a prior holder POSTed this refresh token and we never learned the outcome —
      # it may already be consumed; re-sending it is the reuse that revokes the family.
      fail closed → "re-auth required" (do NOT send it)
  if access token still valid AND generation advanced past our snapshot:
      adopt it, return                       # ← coalesce: the loser makes NO upstream call
  # rotate, exactly once:
  write+fsync `pending = {generation, fingerprint(refresh)}`   # consumed-marker BEFORE the POST
  POST /oauth/token with the current refresh token (bounded timeout)
    success:                 splice new pair, bump generation, CLEAR pending, atomic write, return new
    definite 4xx invalid_grant: clear pending, mark family revoked → "re-auth required"
    AMBIGUOUS (timeout / 5xx / conn drop after send):
        leave pending SET, do NOT retry this token, do NOT clear
        → continue on the still-valid access token if any; the next refresh fails closed → re-auth
```

The `pending` marker (fsync'd under the lock *before* the POST) is what makes
"at most once" survive a crash, a timeout, or a wedged holder: no other broker, and
no retry, ever re-sends a token that might already be consumed. This is stricter
than the doc's earlier "worst case is a stale token" — under strict reuse
detection, re-sending a maybe-consumed token is the failure, so the safe response
to an *ambiguous* refresh is re-auth, not retry.

**Convoy control.** The lock is held across the upstream POST, so a hung provider
edge serializes everyone behind it. Mitigations are required, not optional:
a **bounded lock-wait** (a waiter that can't acquire in time proceeds on its
current still-valid access token rather than blocking a turn); a **negative cache /
circuit-breaker** so that after one ambiguous/failed refresh, queued workers do NOT
each repeat the doomed POST (without it, `dispatch -k 20` loses ~`k × timeout` ≈ 10
min behind one bad edge); and **lock-owner metadata** (pid/host/started-at) for
operator diagnostics, since OS release-on-death does nothing for a live-but-wedged
or `SIGSTOP`'d holder (notably a reparented libkrun VMM child).

With this, the N per-run brokers behave as **one logical writer**: whoever wins the
lock rotates `RT0→RT1` exactly once; everyone else re-reads, sees the bumped
generation, and adopts the fresh token without ever sending a stale one.

> **Correctness boundary — Primitive 1 stands alone, but only as the protocol
> above, not as a lock around the existing persist.** It serializes rotation
> across brokers and closes the reuse-revoke race independent of any vendor-agent
> assumption — **provided** the in-proxy `/oauth/token` handlers are restructured
> to rotate *through* the `TokenStore` (re-read from disk under the lock; never
> forward the registry's start-of-run token) and the at-most-once `pending` marker
> is enforced. A naive lock-around-the-write keeps the stale-registry-token bug and
> still revokes. **Primitive 2 is an optimization layered on top, and it carries an
> unverified assumption (below) — so M1 must be shippable on Primitive 1 alone,
> with Primitive 2 gated behind an
> empirical check.** If that check fails, we keep Primitive 1 (correct, but the
> guest still refreshes through the broker, so the libkrun response-leg gap is
> handled separately rather than sidestepped).

### Primitive 2 — the non-expiring stub (OPTIMIZATION, empirically gated)

> **This rests on an assumption the codebase cannot prove** (Codex review,
> 2026-06-19): nothing in `src/` establishes whether Claude Code / Codex decide to
> refresh off the **creds-file** `expiresAt`/`last_refresh` field or off an
> internal JWT `exp` / timer. pillbox's own pre-refresh reads Claude's `expiresAt`
> (`vault/refresh.rs`), but that's *pillbox* reading it, not the agent. So this
> primitive is gated behind an **empirical pre-flight** (Open Questions #1) and is
> NOT a prerequisite for M1 — if the pre-flight fails, we ship Primitive 1 alone
> and address the libkrun response-leg gap directly (the B2 fix) instead.

Today the env-fork stubs the token *value* but leaves the real `expiresAt` in the
stub creds (`libkrun/mod.rs::stub_oauth_creds` rewrites only
`accessToken`/`refreshToken`). So the guest's agent sees a near-future expiry and
**refreshes itself** — which is the in-guest `/oauth/token` path, and on libkrun
that path has a live gap: the MITM is **request-leg-only**
(`libkrun/mitm.rs` swaps stub→real on the request; the response is relayed
verbatim), so a refresh *response* rotates **real** tokens back into the guest —
defeating "the sandbox cannot read credentials."

Fix it at the root: **stub the creds with a far-future `expiresAt`** so the guest
never tries to refresh, and have the broker keep the real access token fresh
**lazily on outbound request, under the `TokenStore` lock**. Then:

- The guest never hits `/oauth/token` → there's no *refresh* response to rewrite →
  that specific leak (a rotation echoing reals into the guest) is gone.
- There is exactly one writer (the broker), and it's serialized by the lock.
- The in-guest refresh race within a single session disappears too.

> **Scope correction (Codex review): this moots the *refresh-token* leak, NOT the
> libkrun response-leg gap in general.** The MITM still relays every other upstream
> response verbatim (`mitm.rs:269`), so a provider response that echoes a
> credential — an authorization-code/login response, an `id_token` (Codex leaves
> `id_token` verbatim in the stub by design, `codex.rs:140`/`:257`), a Set-Cookie —
> still reaches the guest unchanged. Killing the refresh leak does not remove the
> need for the **B2 libkrun response-rewrite fix**; it just removes the
> highest-frequency case.

> **Guest-stub-only invariant.** The far-future `expiresAt` goes ONLY into the
> guest stub file — it must NEVER land in the registry "real" creds or the
> `TokenStore`. If it leaks into the real, `is_at_least_as_fresh`
> (`session.rs:118`) treats a stale `RT0` as newer than disk and `VaultSession::drop`
> overwrites a fresher locked `RT1`. The stub and the real must carry independent
> `expiresAt`.

Per-provider caveat: the agent must key its refresh decision off the **creds-file
`expiresAt`**, not a JWT `exp` it parses from the (stubbed, unreadable) access
token. Claude Code reads the creds-file field; codex's `auth.json` carries
`last_refresh` + opaque tokens. Verify each agent honors the file field before
relying on this (see Open Questions). Where an agent *does* parse a JWT `exp`, the
stub access token must be JWT-shaped with a far-future `exp` (ties into the B2
codex-stub-shape question).

### Every refresh-capable path must route through the `TokenStore` — or be refused

The boundary is **not enforceable by a doc rule**; each path below either rotates
through the single writer or is blocked in code. Today most of them bypass it
(Codex review). This table is the M1a enforcement checklist:

| Path | Today | Required |
|---|---|---|
| `dispatch -k` (libkrun) | doesn't pass `--vault` (`dispatch.rs:603`) | force `--vault`; workers' brokers rotate through the shared-file `TokenStore` |
| `run --detach --vault` (libkrun) | MITM-in-child outlives CLI (`detached_vault: true`, `libkrun/session.rs:52`) | child's refresh routes through the host `TokenStore` lock |
| `run --detach --vault` (docker) | rejected (`docker.rs:146`) **but** detach still bind-mounts the real auth home (`docker.rs:245`) | refuse concurrent OAuth on docker-detach (steer to libkrun) |
| non-vault run (docker fg / any) | bind-mounts/clones real creds → in-guest refresh bypasses the lock | refuse/​warn concurrent OAuth without `--vault` |
| vaulted `--with` without `--vault` | `oauth=None` → real OAuth creds still mounted (`docker.rs:166`) | treat as OAuth-unvaulted → same refusal |
| `codex-serve` | shares Codex auth (`agents/mod.rs:172`), rejects vault in server mode, clones real **unstubbed** (`libkrun/session.rs:760,783`) | refuse concurrent codex-serve OAuth until it can vault |
| multiple projects | share global auth (`agents/mod.rs:356`) | covered iff each is vaulted (rotates through the same per-account `TokenStore`) |

**Enforcement point:** `dispatch` (and the eval rig) must set `--vault` for an
OAuth agent and fail fast if the resolved backend can't honor it; the run dispatch
must reject a concurrent OAuth run that would mount real creds. "Warn" is not
enough for the revoke risk — these are hard refusals.

### `VaultSession::drop` must stop being a second writer

The teardown persist (`session.rs:64`) is an independent writer whose freshness
guard only compares Claude `claudeAiOauth.expiresAt` (`session.rs:118`); for Codex
that field is absent so the guard returns "persist" (`session.rs:124`) and an older
in-memory snapshot can clobber a fresher locked write. **Fix:** route teardown
persistence through the same `TokenStore` lock + `rotation_generation` check for
*every* provider (write back only if `generation ≥ on-disk`), or delete the
drop-writer entirely once the `TokenStore` owns all rotation/persistence.

### Other boundary conditions

- **"Dedicate the account."** External `claude`/`codex` use on the same account
  bypasses pillbox's lock and re-opens the race. Operationalized via **profiles**
  (below), not just a doc warning.
- **`flock` is single-host.** pillbox is local-only, so a local advisory file lock
  is sufficient. (The managed tier solves the same problem with a Credential DO —
  below.)

---

## Local ↔ managed symmetry

The same invariant, two substrates:

| | Local | Managed (Cloudflare) |
|---|---|---|
| Single-writer enforcement | **`TokenStore` file-lock** | **Credential DO** (a DO *is* a single-writer actor) |
| Scope / lifetime | per provider/account/profile, on host | per provider/account/profile, durable |
| Per-run state | the transient vault broker | the **Session DO** (seq/log/attach) — *separate* |
| Token rotation | host-side, lock-serialized | inside the Credential DO |

> **This is a new proposal, not a documented managed-tier direction** (Codex
> review, 2026-06-19): `managed-tier.md`'s Durable Object is the **session
> sequencer** (seq/log/attach), not a credential authority; managed credential
> handling there is "explicitly unresolved" and currently *leans toward CF's own
> secret-injecting proxy* pending parity tests. So the managed single-writer is an
> **open decision with two options:**
>
> - **(a) Our own Credential DO** — a DO is a single-writer actor, so it gives the
>   same "exactly one writer" invariant as the local file-lock, end to end, and
>   keeps the credential boundary *ours* (the portable-capability story). More to
>   build.
> - **(b) Ride CF's credential proxy** — less rebuild, but cedes the boundary to
>   CF and we inherit whatever rotation semantics it has (which must be checked for
>   the same reuse-detection safety).
>
> If we go (a): the Credential DO should be **distinct from the per-run Session
> DO** — session and credential have different lifetimes (session = disposable per
> run; credential = durable per account) — and own token state + rotation + leases,
> handing the Session DO / container short-lived stubs only. A DO migration/eviction
> mid-rotation must be handled with the same hibernate-safe pending-op discipline
> `managed-tier.md` already flags for the DO↔container hop (don't double-rotate
> across a restart).

Either way: **session logs / R2 blobs must never contain raw OAuth tokens or
unredacted provider auth responses** (the security-model at-rest row).

---

## Profiles — operationalizing "dedicate the account"

Add a per-agent auth **profile** so a user can keep a pillbox-dedicated account
distinct from their personal one:

```sh
pillbox auth login --agent claude --profile pillbox-dedicated
pillbox run --agent claude --auth-profile pillbox-dedicated --vault
```

Storage: `~/.pillbox/global/auth/claude/<profile>/…` (default profile keeps the
current path for back-compat). The `TokenStore` lock is **per provider+profile**,
so two different accounts never serialize against each other.

`pillbox auth status --agent claude` surfaces the state that matters:

```
Agent: claude   Profile: pillbox-dedicated   Mode: OAuth
Storage: ~/.pillbox/global/auth/claude/pillbox-dedicated/
Last broker refresh: 2026-06-19 14:22   Rotation generation: 42
Warning: use this account only through pillbox if you rely on OAuth rotation safety.
```

A `pillbox auth refresh --agent claude --profile …` reseed re-runs browser login
to replace a revoked token family and invalidate stale leases.

---

## Build order

- **M1a (now) — the single writer.** `TokenStore` (flock + bounded wait + re-read
  from disk under the lock + atomic single-file write of creds+generation+pending +
  at-most-once `pending` marker + circuit-breaker on ambiguous failure). **Rotate
  the in-proxy `/oauth/token` handlers through it** (anthropic + codex — they must
  stop forwarding the registry's start-of-run token). **Make `VaultSession::drop`
  lock+generation-checked for all providers, or delete it.** Enforce the boundary
  table in code (`dispatch` forces `--vault`; refuse concurrent non-vault OAuth,
  docker-detach OAuth, vaulted-`--with`-without-OAuth, codex-serve). Concurrent +
  ambiguous-failure + no-reuse + crash-mid-rotation tests. **This is the slice that
  kills the dispatch foot-gun — no daemon, no vendor-behavior assumption.**
- **M1b (gated) — the non-expiring stub.** Only after the per-agent pre-flight
  matrix (Open-Q#1). If it passes, stub a far-future expiry (guest-stub-only) so the
  guest stops self-refreshing — removing the refresh-leak case; if it fails, skip it
  and fix the libkrun response rewrite (B2) directly. Codex gets its own pre-flight
  + adapter. **Independent of M1a — never a prerequisite.**
- **M2 — profiles + `auth status`/`doctor` + dedicated-account warning.** Cheap UX
  that operationalizes "dedicate the account."
- **M3 — verify (not "finish") libkrun detached + vault routes refresh through the
  `TokenStore`.** libkrun already advertises `detached_vault: true`
  (`libkrun/session.rs:52`), so the MITM-in-child path exists; M3 is confirming its
  refresh goes through the host lock, not building the transport. (docker
  detached+vault stays unsupported → use libkrun.)
- **M4 (managed) — the open (a)/(b) decision** (own Credential DO vs ride CF's
  proxy). If (a): a Credential DO distinct from the Session DO; container/Session DO
  get stubs only; no tokens in logs/R2; handle DO migration mid-rotation.

**Deliberately skipped:** a resident `vaultd` daemon (the file-lock makes N brokers
safe without one, and it cuts against pillbox's transient-invocation grain — its
only unique value is docker-detached + a single audit log, which don't justify a
daemon); lease stub-hashing + lease TTL (marginal over the existing
session+host-bound registry).

---

## Open questions

1. **Per-provider expiry semantics — the empirical pre-flight that gates Primitive
   2.** Codex confirmed nothing in `src/` proves how the *vendor* agents decide to
   refresh; only pillbox's own pre-refresh reads Claude's `expiresAt`. So before
   Primitive 2 ships, run the test: launch `claude` (then `codex`) **vaulted**,
   with the stub creds carrying a **far-future `expiresAt`/`last_refresh`** while
   the real token underneath is near expiry, and watch for an outbound
   `/oauth/token`. Three outcomes:
   - **Refreshes off the creds-file field** → the non-expiring stub works as
     written. Ship Primitive 2.
   - **Refreshes off a JWT `exp` in the access token** → the stub access token must
     be a JWT-shaped stub with a far-future `exp` (ties into the B2 codex-stub
     shape). Ship Primitive 2 with that change.
   - **Refreshes on its own timer / ignores the field** → Primitive 2 is inert.
     **Fall back to Primitive 1 only** (still correct) and fix the libkrun
     response-leg rewrite directly (the B2 gap) so an in-guest refresh doesn't
     rotate reals into the VM.

   A single happy-path observation is insufficient (Codex review): the test must
   exercise **every trigger** — agent startup, a long idle run crossing the access
   TTL (timer), and a **forced 401** — and the JWT-`exp`-*plus*-file-field case,
   since an agent may refresh on any of them. Run the matrix per agent (Claude and
   Codex independently, Codex keyed on `last_refresh` not `expiresAt`).
2. **Lazy host-side refresh trigger.** With the guest no longer refreshing, the
   broker must refresh the real on demand when an outbound request finds the access
   token within `PRE_EXPIRY_BUFFER` of expiry. Confirm every swap path checks this
   (start-of-run pre-refresh covers boot; long runs need the per-request check).
3. **Lock-held-across-network bound.** Pick the upstream refresh timeout (and a
   max lock-wait) so a hung provider can't park concurrent brokers indefinitely.
4. **Codex diverges from Claude** (Codex review). Codex has its *own* in-proxy
   refresh path (`auth.openai.com`), is **not** wired into the start-of-run
   pre-refresh, leaves `id_token` verbatim, and likely keys expiry off
   `last_refresh` + opaque tokens rather than a Claude-shaped `expiresAt`. The
   `TokenStore` lock protocol is provider-agnostic, but the refresh/expiry adapter
   (`apply_refresh_response`, the expiry read, the pre-refresh wiring) is
   per-provider — don't assume Claude behavior proves Codex behavior. Codex needs
   its own pre-flight (#1) and its own adapter.
5. **Crash safety + at-most-once.** Single-file atomic temp-write + rename (creds +
   `generation` + `pending` together — never two files). Correction (Codex review):
   "worst case is a stale token" is **unsafe** under strict reuse detection — a
   crash/timeout *after* the POST may mean the token was consumed server-side, so
   re-sending it is the reuse that revokes. The `pending` marker (fsync'd before the
   POST) makes the safe recovery be **re-auth, not retry**.

---

## Test plan (M1)

- **Concurrent refresh → one rotation.** Spawn N threads/processes all calling the
  refresh path against one expired `TokenStore`; assert exactly one upstream
  `/oauth/token` call (mocked), all N end with the same fresh token, generation
  bumped once.
- **Adopt-if-fresh.** Second caller after a rotation does *no* upstream call.
- **No-reuse property.** Assert no refresh token value is ever sent to the mock
  upstream more than once across a concurrent burst — including across a simulated
  timeout: a token POSTed then timed-out is never re-sent by any broker or retry.
- **Ambiguous-failure → re-auth, not retry.** Mock a timeout after `.send()`;
  assert `pending` stays set, the next locker fails closed (re-auth), and the same
  token is not re-POSTed.
- **Stale-registry guard.** A broker holding a start-of-run `RT0` whose guest
  refreshes *after* another broker rotated `RT0→RT1` must NOT forward `RT0` — it
  rotates through the `TokenStore` and adopts `RT1`.
- **Drop-writer ordering.** A teardown persist with an older `generation` than disk
  is a no-op (for Codex too, where `expiresAt` is absent).
- **Atomic write / crash.** Kill mid-rotation; creds+generation+pending are one
  file, never torn; recovery re-reads a consistent state.
- **Lock contention bound.** A stuck upstream releases the lock at the timeout; the
  circuit-breaker stops queued workers repeating a doomed POST.
- **Per-profile isolation.** Two profiles refresh independently (no shared lock).

---

## Review history

- **2026-06-19 — Codex adversarial review.** Confirmed the premises (no existing
  `TokenStore`/flock; `snapshot_real` only clones in-memory creds for the teardown
  persist, no in-run disk coordination; managed credentials unbuilt). Landed three
  corrections, now folded in: (1) the non-expiring stub (Primitive 2) rests on an
  assumption the codebase can't prove — split correctness (Primitive 1, ships
  unconditionally) from the optimization (Primitive 2, empirically gated); (2)
  Codex's refresh path diverges from Claude's (own in-proxy path, different expiry,
  not pre-refresh-integrated) — per-provider adapter + its own pre-flight; (3) the
  Credential DO is a *new* proposal, not a documented managed-tier direction
  (which leans toward CF's proxy and is unresolved) — reframed as an open (a)/(b)
  decision.
- **2026-06-19 — Codex round 2 (verdict: has-blocking-holes).** Attacked the
  revised doc; six holes, all cited to code, now folded in: (1) Primitive 1 was
  *not* correct as written — the in-proxy handlers read the refresh token from the
  per-run **registry** (`anthropic.rs:302`/`codex.rs:311`), so a lock around the
  persist still forwards a stale `RT0`; rewrote the protocol to rotate *through* the
  `TokenStore` (re-read from disk) with an **at-most-once `pending` marker** (a
  timeout after POST → re-auth, not retry — re-sending a maybe-consumed token is the
  reuse). (2) **Convoy**: lock-across-network × `k` workers × no negative cache ≈
  `k × timeout`; added bounded lock-wait + circuit-breaker + owner metadata. (3)
  The `--vault` boundary was unenforceable prose → added the **enforcement table**
  (dispatch/docker-detach/non-vault/vaulted-`--with`/codex-serve) as hard refusals.
  (4) `VaultSession::drop` is a second writer whose guard mishandles Codex → route
  it through the lock+generation or delete it. (5) Primitive 2 moots only the
  *refresh* leak, not the libkrun response-leg gap (`id_token`/auth-code/cookies
  still pass through); added the scope correction + guest-stub-only invariant. (6)
  Fixed the creds-path citation (`.claude/.credentials.json`), the
  non-atomic-across-two-files meta (folded `generation`/`pending` into the creds
  file), and the stale M3 (libkrun already `detached_vault: true`).
