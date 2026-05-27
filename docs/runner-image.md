# Runner image

The runner image is the Docker image pillbox launches sandboxes
from. Source lives in [`runner/Dockerfile`](../runner/Dockerfile);
canonical builds are published to GitHub Container Registry on
every tagged CLI release.

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

## Build it yourself

```sh
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t my-team/pillbox-runner:custom \
  runner/

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
- `HOME` is set by the caller (pillbox sets `HOME=/home/lum`
  and bind-mounts the agent's persistent auth state there);
  the image doesn't need to pre-create that path.

## Harness updates

Renovate watches the npm packages pinned in `runner/Dockerfile`
(via `# renovate:` hint comments) and opens PRs on upstream
bumps. CI rebuilds the image on the PR for verification. Patch
+ minor bumps auto-merge on green; major bumps hold for human
review.

After merge to `main`, the `:rolling` tag is republished. The
next CLI release picks up whatever's on `:rolling` and stamps
it as `:vX.Y.Z` + `:latest`.
