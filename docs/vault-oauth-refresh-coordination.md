# Vault OAuth refresh coordination — single-writer token rotation (design)

Status: **partially built** (2026-06-19). A credential-correctness design for
running one subscription OAuth account (Claude, Codex) across **multiple
concurrent pillbox sessions** — `dispatch -k`, concurrent `--detach` runs,
multiple projects — without tripping the provider's refresh-token reuse
detection and getting the whole session logged out.

Implementation status (see [Build order](#build-order) for the slices):

| Slice | What | Status |
|---|---|---|
| M1a core | `TokenStore` single-writer protocol (`vault/token_store.rs`) | **merged** (#95) |
| M1a slice 2a | Claude `RefreshAdapter` + start-of-run pre-refresh routed through `TokenStore`, fail-closed — **both backends** (libkrun `prepare_launch`, docker `provision_oauth_mount`) via the shared `refresh::pre_refresh` | **this PR** |
| M1a slice 2b | in-proxy `/oauth/token` handlers rotate through `TokenStore` (Claude + Codex) | next |
| M1a slice 3 | `VaultSession::drop` teardown persist under the lock (or deleted) | pending |
| M1a slice 4 | enforce the `--vault` boundary table in code (incl. codex-serve unstubbed refusal) | pending |
| M1b / M2 / M3 / M4 | non-expiring stub (gated) / profiles / libkrun verify / managed | later |

### Backend priority — read this before touching the refresh path again

**libkrun is primary; docker is secondary.** This recurs, so it's written down:

- **libkrun** is the local-compute default *and the only backend `dispatch -k`
  uses* — so it's where credential-coordination work matters first. Its pre-refresh
  lives in `libkrun/session.rs::prepare_launch` (the env-fork chokepoint), runs for
  every vault-capable agent (not gated on `--vault`), and therefore covers dispatch
  workers directly.
- **docker** is the no-KVM compat backend **and the local twin of the Cloudflare
  container family** (same container transport family; libkrun is a microVM, a
  different shape). So a correct docker credential flow is the *local rehearsal* of
  the managed backend's flow — "container + a single-writer credential authority" is
  docker+`TokenStore` locally and CF-Container+Credential-DO remotely. That CF-twin
  role — not docker-only users — is why docker survives deprecation here.

**Is any of this relevant to the managed/CF backend?** The **core** (the invariant,
`RefreshAdapter`, the failure classification, the fail-closed policy) is
substrate-agnostic and transfers — *if* M4 builds our own Credential DO (option (a)
below): a DO is the distributed twin of the flock, both "exactly one writer." If M4
instead rides CF's credential proxy (option (b), which `managed-tier.md` currently
leans toward), CF owns rotation and this work stays local-only. The flock and the
per-backend call sites never transfer; the protocol + adapter do. Don't relitigate
docker-vs-libkrun as a *priority* question — they're local twins of different
things, both plugging into the same substrate-agnostic core.

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

Concrete failing timeline (the **pre-2a** behavior this slice closes):

```
Run A and Run B start concurrently, both read RT0 from the global creds file.
A's pre-refresh (the old session.rs → refresh::refresh_real_if_expired): RT0 -> RT1, persist RT1.
B's pre-refresh: still holds RT0 (read before A persisted) -> sends RT0 to provider.
Provider: "RT0 reused" -> revoke the whole family -> A and B both dead -> forced re-login.
```

Slice 2a closes the **pre-refresh** leg of this on **both backends** (libkrun's
`prepare_launch` and docker's `provision_oauth_mount` call the same
`refresh::pre_refresh`): concurrent launches — including `dispatch -k`, which forks
k libkrun workers on the one shared credential — rotate through the single-writer
`TokenStore`, so only one POSTs `RT0` and the rest coalesce onto `RT1`. The
**in-proxy** leg (a guest agent refreshing mid-run, which on libkrun is the
request-leg MITM) is still uncoordinated until slice 2b — see the build order.

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
  (`vault/session.rs::provision_oauth_mount`; as of slice 2a this rotates through
  `TokenStore::ensure_fresh` via `vault/refresh.rs::ClaudeRefreshAdapter`,
  refreshing `PRE_EXPIRY_BUFFER` ahead of `expiresAt`), intercepts in-run
  `/oauth/token` rotations (`vault/providers/anthropic.rs::handle_oauth_request` /
  `handle_response` — **not yet** routed through `TokenStore`, that's slice 2b),
  and persists the rotated pair back to the global creds file.
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

File layout **as implemented** (`TokenStore::new` derives the sidecars from the
creds path; the vendor creds file keeps its own schema untouched):

```
~/.pillbox/global/auth/claude/.claude/
  .credentials.json                       # vendor file — real tokens (unchanged schema)
  .credentials.json.pillbox-rotation.json # pillbox bookkeeping: generation + pending marker
  .credentials.json.pillbox-rotation.lock # flock target — the critical section
```

> **Note — divergence from the original design.** The first draft put
> `generation`/`pending` *inside* the creds file (one atomic write). The
> implementation instead uses a **sidecar** so the vendor file's schema stays
> pristine. Two files can't be updated atomically, so the `pending` marker is
> **reconciled** against the on-disk creds rather than trusted blindly: on entry,
> if `pending`'s fingerprint no longer matches the on-disk refresh token, a prior
> rotation completed and the marker is stale → cleared.
>
> **Known limitation (tracked).** That reconciliation assumes a completed rotation
> *moves* the refresh token. Anthropic does **not** always rotate the refresh
> token (it sometimes returns a new access token only; `apply_refresh_response`
> preserves the old refresh token). In that case a crash in the narrow window
> *after* the durable creds write but *before* the (non-durable) `pending`-clear
> leaves `pending` fingerprinting the still-current refresh token → the next run
> fails closed (spurious re-auth). Safe (never reuse, recoverable via `auth
> login`) but a real false-positive. Fix belongs to the core: fold
> `generation`/`pending` into the durable creds write (per the original design),
> or reconcile on a monotonic signal independent of whether the RT changed.

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

#### Caller policy: fail closed on a non-`Fresh` outcome (slice 2a)

`ensure_fresh` returns `Fresh(creds)` | `ReauthRequired(reason)` | `LockBusy`.
The start-of-run pre-refresh (`provision_oauth_mount`) **aborts the run on
anything but `Fresh`** rather than proceeding best-effort on the stored token.
The reasoning is the at-most-once invariant, not just UX:

- The only safe credential to lease into the proxy is a freshly-rotated one. A
  stale, refresh-capable credential lets the guest agent re-POST a maybe-consumed
  / being-rotated refresh token through the **in-proxy** path (still uncoordinated
  until slice 2b) and trip reuse-revoke.
- A refresh is only attempted when the access token is already past expiry, so a
  failed refresh means the run would 401 anyway — aborting with a clear
  `pillbox auth login` / "retry" message beats a cryptic mid-run failure.
- `LockBusy` only fires when a holder is genuinely wedged (the bounded wait is
  sized above the POST timeout; a normal convoy coalesces in well under it), and a
  wedged holder is one that's mid-POST of a *stale* token — so this caller's token
  is stale too. Fail closed.

This is stricter than the doc's earlier "best-effort, agent's retry-on-401
recovers" framing: best-effort proceed is the unsafe bit while the in-proxy path
(2b) is uncoordinated.

#### Implementation refinements (slice 2a review)

The `RotateError` classification is the at-most-once hinge — only a *provably
non-consuming* failure may be `Definite` (which clears the `pending` marker):

- **Redirects disabled** on the refresh client. reqwest replays the POST body on
  307/308, so following a redirect would POST the same refresh token twice in one
  call and trip reuse detection. A 3xx surfaces as a non-2xx → `Ambiguous`.
- **`Definite` only for a recognized OAuth grant-rejection** (RFC 6749 §5.2
  `invalid_grant` etc.) **or a pre-send connect error** (`is_connect() &&
  !is_timeout()` — a connect *timeout* can never be proven pre-send). A 429, a
  middlebox 4xx, a 5xx, a timeout, or an unparseable body → `Ambiguous`: a blanket
  `is_client_error()` was unsafe because a 4xx returned *after* the server rotated
  would clear `pending` and re-enable a consumed token.
- **No raw response bodies in surfaced reasons** — only the status + the short
  OAuth error code, so a reflected request parameter can't echo a token into logs.
- **`write_atomic` fsyncs the parent directory** after the rename on durable
  writes, so a crash can't lose the `pending` rename after the POST.

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

- **M1a — the single writer.** Split into reviewable slices:
  - **core (merged, #95)** — `TokenStore` (flock + bounded wait + re-read from
    disk under the lock + atomic single-file write + at-most-once `pending` marker
    fsync'd before the POST + coalesce). Behavior-inert until wired.
  - **slice 2a (this PR)** — Claude `RefreshAdapter` + the start-of-run
    pre-refresh routed through `ensure_fresh`, **fail-closed** on non-`Fresh`, with
    the redirect/grant-rejection/connect-vs-timeout/dir-fsync refinements above.
    Wired into **both backends** via the shared `refresh::pre_refresh`: libkrun's
    `prepare_launch` (the env-fork chokepoint — covers `dispatch -k`) and docker's
    `provision_oauth_mount`. Closes the **pre-refresh** leg of the dispatch
    foot-gun — no daemon, no vendor-behavior assumption.
  - **slice 2b (next)** — rotate the in-proxy `/oauth/token` handlers through the
    `TokenStore` (anthropic + codex must stop forwarding the registry's
    start-of-run token; synthesize the guest response from the locked result).
    Closes the **in-proxy** leg. *(Review carry-overs from 2a: (a) if this second
    `ensure_fresh` caller needs to branch on clean-rejection vs maybe-consumed,
    promote `ReauthRequired(String)` to carry that bit structurally instead of in
    prose — 2a's sole caller fails closed on both; (b) the two backends key
    `pre_refresh` differently — libkrun on `spec.auth_id` (the credential owner),
    docker on `agent.agent_id` — identical for claude but they diverge for an alias
    whose `auth_id != id` (the codex-serve shape codex 2b introduces); converge
    both on the credential owner.)*
  - **slice 3** — make `VaultSession::drop` lock+generation-checked for all
    providers, or delete it once the `TokenStore` owns all rotation/persistence.
    *(Review carry-over: fold the seconds↔ms `expiresAt` normalization into one
    shared `expires_at_ms` helper — `refresh::is_expired` and `session.rs`'s
    teardown copy duplicate it today; this path is the natural place since it
    reworks `is_at_least_as_fresh`.)*
  - **slice 4** — enforce the boundary table in code (`dispatch` forces `--vault`;
    refuse concurrent non-vault OAuth, docker-detach OAuth,
    vaulted-`--with`-without-OAuth, codex-serve).
  - Convoy control (negative cache / circuit-breaker) lands with 2b, where the
    `k`-worker in-proxy contention actually occurs.
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

- **2026-06-19 — slice 2a round 2 (structural + Codex `gpt-5.5` xhigh, on the
  post-fix shape).** Caught the scope bug: 2a as first written wired only docker's
  `provision_oauth_mount`, but **libkrun has no `VaultSession`** (it uses the
  `stub_oauth_creds` env-fork, which did no pre-refresh) and **dispatch is
  libkrun-only** — so the original "closes the dispatch race" claim was false.
  Fixed by extracting a shared `refresh::pre_refresh` both backends call (docker +
  libkrun `prepare_launch`), making 2a actually cover dispatch. Also folded:
  (1) `clean_rejection` ignored HTTP status → a 5xx/429/3xx with an OAuth-looking
  body could reach Definite; re-added the 400/401 status gate. (2) parent-dir fsync
  was best-effort → made it fail-closed for durable writes (the `pending` marker's
  durability is the invariant). (3) the surfaced OAuth `error` code is now charset/
  length-sanitized (no reflected-value leak). (4) `LockBusy`'s doc was reconciled
  with its fail-closed caller. Deferred (tracked above): structured `ReauthRequired`
  (2b), the expiry-normalization dedup (slice 3).
- **2026-06-19 — slice 2a build review (code-review high + Codex `gpt-5.5` xhigh).**
  Adversarial pass on the *implementation* of the pre-refresh wiring. Findings
  folded in: (1) **redirect double-POST** — reqwest replays the body on 307/308, so
  the refresh client now disables redirects (a 3xx → Ambiguous). (2) **429 /
  middlebox-4xx false-Definite** — a blanket `is_client_error()` → Definite could
  clear `pending` after the server already rotated; tightened to grant-rejection
  error codes only. (3) **fail-closed on non-`Fresh`** — leasing stale
  refresh-capable creds let the agent re-POST through the (uncoordinated) in-proxy
  path; the pre-refresh now aborts the run on `ReauthRequired`/`LockBusy`/error.
  (4) **secret-leak sanitize** — surfaced reasons carry status + OAuth error code
  only, never the raw response body. (5) **connect-vs-timeout** — `Definite` for a
  connect error excludes a connect timeout. (6) **dir fsync** — `write_atomic`
  fsyncs the parent directory after the rename so a crash can't lose the `pending`
  marker. Confirmed clean: the `e.is_connect()` pre-send semantics, the
  `needs_refresh`-on-absent-`expiresAt` coalesce, and that deleting
  `refresh_real_if_expired` lost no behavior. Carried forward (not 2a): the
  teardown writer outside the lock = slice 3; in-proxy coordination = slice 2b.
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
