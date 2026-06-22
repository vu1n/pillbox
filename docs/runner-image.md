# Runner image

> **Note (2026-06-01):** the image *contents* (the bundled agents + tools) carry
> forward, but the **Docker-container framing** below is deprecated — under the
> [libkrun pivot](./libkrun-sandbox.md) the OCI image becomes a **microVM rootfs**
> (krunvm/crun-krun style), or a slimmer custom rootfs. Build/publish mechanics
> change; what's *in* the image mostly doesn't.

The runner image is the Docker image pillbox launches sandboxes
from. Source lives in [`runner/Dockerfile`](../runner/Dockerfile);
canonical builds are published to GitHub Container Registry on
every tagged CLI release.

> **Forward note:** image size is currently an *estimate* (nothing measures it —
> add a CI image-size check). Image slimming (Wolfi/distroless + eStargz/SOCI
> lazy-pull) and a `doctor` host↔image version-compat check are on the
> [remotes-redesign](./archive/remotes-redesign.md) roadmap; the cold `docker pull` is
> the BYO first-run cost to beat.

## What's in it

Five agent CLIs preinstalled at known paths:

Every harness is **pinned** to a concrete version (an `ARG …_VERSION` in
`runner/Dockerfile`) so Docker's layer cache reflects the version we ask for —
an unpinned `@latest` rides a `RUN` whose command string never changes, so a
rebuild silently reuses the stale layer instead of pulling the newer agent.

| Harness | Install method | Pin | Tracked by Renovate |
|---|---|---|---|
| claude | native installer from `claude.ai/install.sh` (`claude install <ver>`) | `CLAUDE_VERSION` | yes — npm `@anthropic-ai/claude-code` (versions match the native release) |
| codex | native installer from `chatgpt.com/codex/install.sh` | `CODEX_VERSION` | yes — github releases (`rust-v<ver>`) |
| amp | `npm i -g @ampcode/cli@<pinned>` | `AMP_VERSION` | no — timestamp+sha versions defeat semver; bump by hand |
| opencode | `npm i -g opencode-ai@<pinned>` | `OPENCODE_VERSION` | yes — npm |
| pi | `npm i -g @earendil-works/pi-coding-agent@<pinned>` | `PI_VERSION` | yes — npm |

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

## Tags published

| Tag | Cadence | Notes |
|---|---|---|
| `vX.Y.Z` | per CLI release | matches `CARGO_PKG_VERSION`. Most stable. |
| `latest` | per CLI release | alias for the most recent `vX.Y.Z`. The default. |
| `rolling` | per Dockerfile merge to main | rebuilt anytime Renovate bumps a harness version. Bleeding edge — opt in via override. |

## Updating the bundled agents

[`scripts/build-runner.sh`](../scripts/build-runner.sh) is the one-stop wrapper —
it resolves each agent's latest version, rewrites the pins, rebuilds, and prints
the versions baked into the image.

```sh
scripts/build-runner.sh --update --dry-run   # show what would change, no write/build
scripts/build-runner.sh --update             # bump all agents to latest, rebuild, verify
scripts/build-runner.sh                       # rebuild current pins (layer-cached), verify
scripts/build-runner.sh --update --tag pillbox-runner:l8
```

`--update` edits the `ARG …_VERSION` pins in `runner/Dockerfile`, so it's a
tracked change: review `git diff runner/Dockerfile` and commit it like a Renovate
bump. Layer caching keeps the rebuild partial — apt / Node / the cargo-built
`pillbox` layers stay cached; only the bumped agent layers recompile. The new
image gets a new id, so libkrun re-materializes its rootfs on the next run (add
`--prune-rootfs` to drop the superseded generation under `~/.pillbox/krun/rootfs/`).

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

Renovate watches the npm packages pinned in `runner/Dockerfile`
(via `# renovate:` hint comments) and opens PRs on upstream
bumps. CI rebuilds the image on the PR for verification. Patch
+ minor bumps auto-merge on green; major bumps hold for human
review.

After merge to `main`, the `:rolling` tag is republished. The
next CLI release picks up whatever's on `:rolling` and stamps
it as `:vX.Y.Z` + `:latest`.
