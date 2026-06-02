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

| Harness | Install method | Tracked by Renovate |
|---|---|---|
| claude | native installer from `claude.ai/install.sh` | yes (release feed) |
| codex | `npm i -g @openai/codex@<pinned>` | yes |
| amp | `npm i -g @ampcode/cli@latest` | no — timestamp+sha versions |
| opencode | `npm i -g opencode-ai@<pinned>` | yes |
| pi | `npm i -g @earendil-works/pi-coding-agent@<pinned>` | yes |

Plus the system tooling agents tend to reach for: `bash`,
`bubblewrap`, `ca-certificates`, `curl`, `gh`, `git`, `jq`,
`openssl`, `python3`, `ripgrep`, `tmux`, `xz-utils`, Node 22 LTS.

And **`pillbox` itself** at `/usr/local/bin/pillbox`, compiled from the
repo in a multi-stage build. The in-sandbox pillbox runs the interactive
attach pty-host (`pillbox pty-host`), the per-attach relay (`pillbox
pty-relay`), and the event emitter / `session done` wrapper — the same
in-sandbox role the e2b/ssh backends already rely on. Because the image
embeds the binary, it is rebuilt when `src/**` or `Cargo.{toml,lock}`
change, not only on `runner/Dockerfile` edits.

## Picking which image pillbox uses

Resolution order (highest precedence first):

1. **`PILLBOX_RUNNER_IMAGE` env var** — one-off override per
   invocation, scriptable from CI.
2. **`[runner] image = "…"` in `pillbox.toml`** — per-pillbox
   pin, checked into the repo.
3. **Built-in default** — `ghcr.io/vu1n/pillbox-runner:rolling`
   during prerelease. CI only moves `:latest` on a *stable* semver
   release, so pre-1.0 `:latest` stays frozen at an old build (the bug
   that shipped installs without iproute2/pip — egress broken).
   `:rolling` is the deliberate dev build, published by a manual
   `workflow_dispatch` (see below), so a fresh install tracks the last
   intentionally-published runner. Repin to `:latest` at the first
   stable release.

`pillbox doctor` shows the resolved image + the source.

## Tags published

| Tag | Cadence | Notes |
|---|---|---|
| `vX.Y.Z` | per CLI release | matches `CARGO_PKG_VERSION`. Most stable. |
| `latest` | stable semver release only | alias for the most recent *stable* `vX.Y.Z`. Frozen during prerelease — not the default until 1.0. |
| `rolling` | per manual `workflow_dispatch` | the deliberate dev build (a dispatch runs from `main` → publishes `:rolling`). **The prerelease default.** Not auto-rebuilt — run the workflow when you want a fresh image. |

## Build it yourself

```sh
# Context is the repo root (the build compiles the in-sandbox pillbox);
# point -f at the Dockerfile.
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
bumps. The image is NOT auto-rebuilt on those PRs (or on merge to
main) — `runner-image.yml` triggers only on `v*` tags + a manual
`workflow_dispatch`, deliberately, since per-push rebuilds of the
~2GB image were churning for no consumer.

To publish a fresh image: run the `runner-image` workflow manually
(`workflow_dispatch` → `:rolling`) or push a `v*` tag (→ `:vX.Y.Z`,
plus `:latest` on a stable release). Or build + push from a dev box
(`DOCKER_HOST=ssh://… docker buildx build -f runner/Dockerfile
--push -t ghcr.io/vu1n/pillbox-runner:rolling .`).
