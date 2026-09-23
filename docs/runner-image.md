# Runner image

> **Note (2026-06-01):** the image *contents* (the bundled agents + tools) carry
> forward, but the **Docker-container framing** below is deprecated — under the
> [libkrun pivot](./libkrun-sandbox.md) the OCI image becomes a **microVM rootfs**
> (krunvm/crun-krun style), or a slimmer custom rootfs. Build/publish mechanics
> change; what's *in* the image mostly doesn't.

The runner image is the OCI source that libkrun materializes as a microVM rootfs. Source lives in [`runner/Dockerfile`](../runner/Dockerfile);
canonical builds are published to GitHub Container Registry on
every tagged CLI release.

> **Forward note:** image size is currently an *estimate* (nothing measures it —
> add a CI image-size check). Image slimming (Wolfi/distroless + eStargz/SOCI
> lazy-pull) and a `doctor` host↔image version-compat check are on the
> [remotes-redesign](./archive/remotes-redesign.md) roadmap; the cold `docker pull` is
> the BYO first-run cost to beat.

## What's in it

Seven agent CLIs preinstalled at known paths:

Every harness is **pinned** to a concrete version (an `ARG …_VERSION` in
`runner/Dockerfile`) so Docker's layer cache reflects the version we ask for —
an unpinned `@latest` rides a `RUN` whose command string never changes, so a
rebuild silently reuses the stale layer instead of pulling the newer agent.

| Harness | Install method | Pin | Tracked by Renovate |
|---|---|---|---|
| claude | native installer from `claude.ai/install.sh` (`claude install <ver>`) | `CLAUDE_VERSION` | yes — npm `@anthropic-ai/claude-code` (versions match the native release) |
| codex | native installer from `chatgpt.com/codex/install.sh`; complete native package preserved under `/opt/codex/packages/standalone/releases/<version>` | `CODEX_VERSION` | yes — github releases (`rust-v<ver>`) |
| cursor | official `cursor.com/install` artifact | `CURSOR_AGENT_VERSION` | no — resolved from the official installer by `build-runner.sh --update` |
| amp | `npm i -g @ampcode/cli@<pinned>` | `AMP_VERSION` | no — timestamp+sha versions defeat semver; bump by hand |
| opencode | `npm i -g opencode-ai@<pinned>` | `OPENCODE_VERSION` | yes — npm |
| pi | `npm i -g @earendil-works/pi-coding-agent@<pinned>` | `PI_VERSION` | yes — npm |
| prime-agent | official native installer, checksummed release archive under `/opt/prime-agent` | `PRIME_AGENT_VERSION` | no — stable feed via `build-runner.sh --update` |

Prime Agent is bundled as a command-line tool. It is not yet registered as a
Pillbox agent: `--agent prime-agent`, managed auth, and structured session events
require a separate adapter. Its Python kernel dependencies are not pre-provisioned;
workflows using that kernel still need an explicit setup step and network access.
The complete native release lives outside the runtime HOME so its assets survive
home-directory mounts.

The September 23 refresh pins Claude 2.1.280, Codex 0.156.1, Pi 0.87.1, and
Prime Agent 0.9.5. Other bundled harnesses retain their existing pins. Updating the
daily-use runner does not migrate a sealed execution profile. A profile that
requires Codex 0.151.0 must keep its matching immutable image until its adapter
and protocol have been qualified against a newer release.

Plus the system tooling agents tend to reach for: `bash`,
`bubblewrap`, `ca-certificates`, `curl`, `gh`, `git`, `iproute2`
(the `ip` tool the libkrun egress fence needs), `jq`, `openssl`,
`python3`, `ripgrep`, `tmux`, `xz-utils`, Node 22 LTS.

And **`pillbox` itself** at `/usr/local/bin/pillbox`, compiled from the
repo in a multi-stage build. The in-sandbox pillbox runs the interactive
attach pty-host (`pillbox pty-host`), the per-attach relay (`pillbox
pty-relay`), and the event emitter / `session done` wrapper — the
in-sandbox role both local backends rely on. Because the image
embeds the binary, it is rebuilt when `src/**` or `Cargo.{toml,lock}`
change, not only on `runner/Dockerfile` edits.

The Codex installer package is kept intact at
`/opt/codex/packages/standalone/releases/<CODEX_VERSION>`. Its manifest, native
`bin/codex`, `bin/codex-code-mode-host`, bundled `codex-resources`, and
`codex-path/rg` survive cleanup of the installer's scratch home. The two native
executables are symlinked into `/usr/local/bin`; `scripts/build-runner.sh`
checks these companions and the pinned manifest without making an authenticated
model call.

## Picking which image pillbox uses

Resolution order (highest precedence first):

1. **`PILLBOX_RUNNER_IMAGE` env var** — one-off override per
   invocation, scriptable from CI.
2. **`[runner] image = "…"` in `pillbox.toml`** — per-pillbox
   pin, checked into the repo.
3. **Built-in default** — `ghcr.io/vu1n/pillbox-runner:latest`
   today. Bumps per pillbox-CLI release so a fresh install picks
   up a matching pre-published image.

`pillbox doctor` shows the resolved image + the source.

## Tags

Tags name **roles, not history** — there is no `l5`/`l6`/`l7` generation scheme
(that conflated "libkrun dev phase" with "the image you run" and caused endless
"which tag is current?" churn; see [decisions.md](./decisions.md)). Three tags,
plus immutable versions:

| Tag | Moving? | Cadence | Use it for |
|---|---|---|---|
| `dev` | moving | local `build-runner.sh`; manual CI dispatch on main | day-to-day dev — pin it in your dev `pillbox.toml` |
| `latest` | moving | CI on stable release (alias of newest `vX.Y.Z`) | prod / fresh installs — the built-in default |
| `vX.Y.Z` | **immutable** | CI per release | reproducibility — pin this (and *only* this) for a frozen eval/σ̂ baseline |

So you pin `dev` or `latest` and never chase a number. When you need a run to be
reproducible, pin a concrete `vX.Y.Z` — that's what versions are for.

## Updating the bundled agents

[`scripts/build-runner.sh`](../scripts/build-runner.sh) is the one-stop wrapper —
it resolves each agent's latest version, rewrites the pins, rebuilds, and prints
the versions baked into the image.

```sh
scripts/build-runner.sh --update --dry-run   # show what would change, no write/build
scripts/build-runner.sh --update             # bump all agents to latest, rebuild, verify
scripts/build-runner.sh                       # rebuild current pins (layer-cached), verify
scripts/build-runner.sh --tag pillbox-runner:v0.2.0   # build an immutable version tag
```

`--update` edits the `ARG …_VERSION` pins in `runner/Dockerfile`, so it's a
tracked change: review `git diff runner/Dockerfile` and commit it like a Renovate
bump. Layer caching keeps the rebuild partial — apt / Node / the cargo-built
`pillbox` layers stay cached; only the bumped agent layers recompile. The new
image gets a new id, so libkrun re-materializes its rootfs on the next run (add
`--prune-rootfs` to drop superseded current-format generations for that exact
image reference under `~/.pillbox/krun/rootfs/`).

libkrun's materialized cache format is versioned independently of image tags.
The current `v3` layout is
`rootfs/v3/<sha256-image-ref>/<sanitized-image-id>/{.materialized,rootfs/}`. Only the
`rootfs/` child is served to the guest; the sibling authority marker is read as
a bounded, no-follow regular file and binds the exact format, original image
reference, and image ID. The hash namespace prevents distinct valid image refs
from aliasing through lossy filename sanitization. Extraction preserves archive
permissions, including `/tmp`'s required `01777` mode.

Docker-unavailable fallback accepts only an exact current-format namespace and
marker. Launch never deletes or rewrites a pre-existing generation because a
running VM serves its rootfs directory live. Explicit `--prune-rootfs` considers
only generations beneath the exact current v3 image-ref hash; legacy and v2
directories remain untouched because their guest-writable metadata cannot
authorize deletion. Materialized generations remain beneath Pillbox's
host-owned `~/.pillbox` directory, whose mode is reasserted as `0700` on every
access; preserved setuid/setgid bits are therefore not exposed through a shared
cache.

## Build it yourself

```sh
# Context is the repo root (the build compiles the in-sandbox pillbox);
# point -f at the Dockerfile. Single native arch + --load for the local loop
# (build-runner.sh does this); the multi-arch form below is for publishing.
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t my-team/pillbox-runner:custom \
  -f runner/Dockerfile .

PILLBOX_RUNNER_IMAGE=my-team/pillbox-runner:custom pillbox run
```

## Layer your own tools on top

The cleanest way to add tools (extra agents, language runtimes,
internal scripts) is to base a derived image on the canonical
runner:

```dockerfile
FROM ghcr.io/vu1n/pillbox-runner:vX.Y.Z

RUN apt-get update \
    && apt-get install -y --no-install-recommends my-tool another-tool \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @my-org/my-agent@1.2.3
```

Then point pillbox at it via env or `pillbox.toml`. The base
image's contract (paths, system tools, HOME convention) carries
through automatically.

## Contract a custom image must satisfy

If you build from scratch instead of layering on the canonical
image, pillbox CLI assumes:

- Agent binaries on `$PATH` — at minimum `claude` and/or `codex`
  for the agents you intend to run. `pillbox doctor` will flag
  missing ones at runtime.
- `/workspace` exists and is writable (bind-mount target).
- `/tmp` exists with mode `01777`; libkrun preserves image archive mode bits
  when materializing the rootfs.
- `/etc` writable for the `--mcp-config` bind mount.
- A shell.
- `HOME` is set by the caller (pillbox sets `HOME=/home/pillbox`
  and bind-mounts the agent's persistent auth state there);
  the image doesn't need to pre-create that path.
- `pillbox` on `$PATH` — the interactive attach transport launches
  `pillbox pty-host` / `pillbox pty-relay` inside the sandbox. A
  version skew between host and in-sandbox pillbox is tolerated within
  a frame `PROTO_VERSION`; layer on the canonical image to stay matched.
- `update-ca-certificates` available **and** an entrypoint that runs
  it when a CA is mounted at
  `/usr/local/share/ca-certificates/pillbox-vault.crt`. Pillbox's
  vault session bind-mounts the per-run CA there so non-Node agents
  (Codex's reqwest, future Rust/Go agents) honor the MITM cert via
  the system trust store. Node agents go through
  `NODE_EXTRA_CA_CERTS` and don't need this. The canonical image
  ships `runner/entrypoint.sh` as `ENTRYPOINT` — custom images
  should either copy the same script or replicate its behavior.

## Harness updates

Renovate watches supported package/release pins in `runner/Dockerfile`
(via `# renovate:` hint comments) and opens PRs on upstream bumps. Cursor and
Amp are refreshed by `build-runner.sh --update` from their official installer
and npm metadata respectively. The current runner-image workflow runs only on version tags or manual dispatch;
it does not build pull requests or republish on every merge. Verify a local build
before relying on changed pins. The next tagged release publishes the immutable
version and stable `:latest` alias.


## Proposed launch-time updates (not implemented)

Decouple harness packages from the base OS image. At launch, the host would resolve
an update policy before creating the session or sealing an execution request:

- `pinned`: use exactly the requested version and digest; no update lookup.
- `cached` (proposed interactive default): use a previously verified package,
  check for updates at most once per day, and prepare newer packages for later sessions.
- `latest`: resolve and prepare the current stable version before starting; a failed
  update is an explicit launch failure, never an undisclosed older-version fallback.

Packages would be cached by harness, platform, version, and content digest. A
single updater stages and verifies a complete package in isolation, then publishes
it atomically. Every new session records the selected package version/digest and
base image ID. Running sessions keep their original package. Never install into a
shared materialized rootfs or update an active invocation in place.

This removes the image-release dependency without reinstalling four harnesses on
every launch. Cached/offline launches should disclose the selected version and any
failed freshness check. Updates need no user credentials; installation runs in a
separate preparation environment. Sealed execution profiles remain pinned and
require adapter compatibility qualification before accepting a newer package.

The smallest first implementation is an explicit host-managed `harness update`
operation backed by this cache, followed by optional freshness checks at launch.
Package mounting, cache ownership, session provenance, and failure semantics need
a runtime contract before implementation; this image refresh adds none of those
runtime behaviors.
