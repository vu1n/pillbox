# Recipes

Copy-paste flows for common tasks. For the full command reference see
[AGENTS.md](../AGENTS.md); for the why behind secrets see
[secrets.md](./secrets.md).

All snippets assume `pillbox doctor` is green — run it first when you're
in a new shell.

## First-time setup on a new machine

```sh
# Build / install (until pillbox publishes its image to GHCR)
cd ~/code/lum && bun run build:runtime-image:pillbox
cd ~/code/pillbox && cargo install --path .

# Verify
pillbox doctor

# Bootstrap the global pillbox
pillbox init

# Authenticate (writes to global, shared across projects)
pillbox auth login --agent claude    # OAuth in browser
pillbox auth login --agent codex     # device code (URL + code printed)

# Confirm
pillbox auth list

# Set up a project pillbox
cd ~/work/myapp
pillbox new --name myapp
```

## One agent, one API key

The simplest possible flow.

```sh
pillbox secret add ANTHROPIC_API_KEY              # paste, then Ctrl-D
pillbox run --with ANTHROPIC_API_KEY
```

Inside the sandbox, claude sees `$ANTHROPIC_API_KEY` set to the value
you pasted. cwd is mounted at `/workspace/<basename(cwd)>`.

## Targeting dev / staging / prod

Load three bundles once, pick one per run.

```sh
pillbox env load dev   ~/work/myapp/.env.dev
pillbox env load stage ~/work/myapp/.env.staging
pillbox env load prod  ~/work/myapp/.env.prod

pillbox run --env dev
pillbox run --env stage
pillbox run --env prod
```

To override a single variable for one run (e.g. swap in your personal
API key):

```sh
pillbox run --env prod --with ANTHROPIC_API_KEY
# pillbox: note: ANTHROPIC_API_KEY shadowed by --with ANTHROPIC_API_KEY
```

## Pulling secrets from the host environment

When the value is already in your shell (e.g. CI, direnv), don't paste:

```sh
# $ANTHROPIC_API_KEY is already exported in this shell
pillbox secret add ANTHROPIC_API_KEY --from-env ANTHROPIC_API_KEY
```

The `--from-env` flag names the host variable to read; the positional
arg is what to store it as.

## Idempotent setup script

Safe to re-run on every boot. `--if-not-exists` makes both `secret add`
and `env load` fail with exit 1 if the name is already taken, which
combined with `|| true` becomes a no-op-on-second-run pattern.

```sh
#!/usr/bin/env bash
set -e
pillbox doctor
pillbox secret add ANTHROPIC_API_KEY --from-env ANTHROPIC_API_KEY --if-not-exists || true
pillbox env load prod ~/work/myapp/.env.prod --if-not-exists || true
echo "pillbox ready"
```

If you want hard failure on conflict, drop the `|| true`.

## Mounting an extra directory into the sandbox

`--mount` is forwarded straight to `docker -v`, so the syntax is
`HOST:GUEST[:opts]`.

```sh
# Read-only AWS creds
pillbox run --mount ~/.aws:/home/pillbox/.aws:ro

# Sibling repo at /workspace/sibling (in addition to cwd)
pillbox run --mount ~/work/sibling-repo:/workspace/sibling

# SSH agent socket (Linux). Stash the var name first; pillbox looks
# it up at run time so the recipe stays sandbox-safe.
pillbox secret add SSH_AUTH_SOCK --from-env SSH_AUTH_SOCK
pillbox run --mount $SSH_AUTH_SOCK:/ssh-agent --with SSH_AUTH_SOCK=SSH_AUTH_SOCK
```

The agent's working directory inside the sandbox is always
`/workspace/<name>` where `name` defaults to `basename(cwd)` (override
with `--name`).

## Multi-repo workspace

```sh
cd ~/work/primary
pillbox run \
  --name primary \
  --mount ~/work/lib-a:/workspace/lib-a \
  --mount ~/work/lib-b:/workspace/lib-b \
  --env stage
```

Inside the sandbox: `/workspace/primary`, `/workspace/lib-a`,
`/workspace/lib-b`, all writable. The agent starts in `primary`.

## Reading a stored secret out for another tool

```sh
ANTHROPIC_API_KEY=$(pillbox secret show ANTHROPIC_API_KEY --reveal --to-stdout) \
  ./some-tool-that-needs-the-key
```

`--to-stdout` is required because stdout isn't a TTY here — pillbox
refuses to leak revealed values into a pipe without you saying so
out loud.

## Inspecting state from a script

```sh
# What's authenticated?
pillbox auth list --json | jq '.agents[] | select(.authenticated)'

# What secrets exist?
pillbox secret list --json | jq -r '.secrets[].name'

# What bundles exist + their sizes?
pillbox env list --json | jq '.bundles[]'

# Is the environment ready?
pillbox doctor --json | jq '.overall_ok'
```

Every `--json` payload includes a top-level `version: 1`. Pin against
that — fields will be added freely, the version bumps on restructure.

## Running on a registered VPS over SSH

```sh
# One-time: install pillbox on the VPS (we don't deploy binaries).
ssh deploy@vps.example.com 'cargo install --git https://github.com/vu1n/pillbox'

# Register it locally.
pillbox remote add prod-vps ssh://deploy@vps.example.com

# A remote run needs an S3-shaped workspace backend; the VPS pulls
# from the same bucket, so no workspace bytes cross SSH.
pillbox new --workspace-backend s3 \
  --bucket my-snapshots --endpoint https://acct.r2.cloudflarestorage.com \
  --access-key-env R2_ACCESS_KEY --secret-key-env R2_SECRET_KEY

pillbox run --remote prod-vps
```

Real secret values cross the network once over the SSH-encrypted
channel, into the remote pillbox's vault session memory. Nothing is
written to disk on the remote.

## Running on an E2B managed sandbox

```sh
# Prereqs (one-time, local machine):
brew install node               # or use your OS package manager
npm i -g e2b                    # the JS SDK; pillbox spawns it via Node
export E2B_API_KEY="e2b_…"      # or pillbox secret add E2B_API_KEY

# Register the template (you've baked pillbox into the image yourself).
pillbox remote add prod-cloud e2b://my-pillbox-template

pillbox run --remote prod-cloud
```

The first remote run extracts the embedded Node helper to
`~/.pillbox/cache/e2b-helper-v<pkg>.mjs` (0700) and uses it for
subsequent runs. `pillbox version` upgrades replace the cached
helper automatically — older versioned files sit on disk until you
`rm ~/.pillbox/cache/e2b-helper-*`.

## Detached background session + reattach

```sh
# Start, immediately return — agent keeps running in the background.
pillbox run --remote prod-cloud --detach --label "nightly-refactor"
# pillbox: ✓ session `abc123def456` started in background (sandbox `sb_xxx`).
#          pillbox session attach abc123def456  # reattach

# Browse:
pillbox session list                       # human
pillbox session list --json                # machine

# Reattach. Ctrl-A D detaches again without killing.
pillbox session attach abc123def456

# Detach from another shell (no need to be the attached terminal).
pillbox session detach abc123def456

# Tear down for good (kills the sandbox + removes the record).
pillbox session rm abc123def456
```

Ids accept any unique prefix ≥ 4 chars (`pillbox session attach
abc1` works if there's only one match). Ambiguous prefixes list the
candidates in the error.

## Snapshot / restore the workspace

Every project pillbox owns one rustic repository (encrypted,
content-addressed). Snapshots are cheap; the encryption password
lives at `<state_dir>/repo-password` (0600, local-only) so a stolen
S3 bucket alone can't be decrypted.

```sh
# Snapshot cwd into the repo (rustic dedupes against prior snapshots).
pillbox push --tag "before-refactor" --message "experiment in scope"

# List what's in the repo.
pillbox snapshot list

# Restore — defaults to the latest snapshot.
pillbox pull

# Restore a specific snapshot by handle prefix.
pillbox pull --snapshot abcd

# Bookmark a snapshot and restore from the bookmark later.
pillbox bookmark set main latest
pillbox pull --bookmark main

# Start a remote run from a bookmark instead of snapshotting cwd first.
pillbox run --remote cloud --from-bookmark main

# Rotate the encryption password (old key still works until upstream
# lands deletion — treat as compromised).
pillbox workspace rekey
```

For an S3-backed pillbox the snapshots land in your bucket; for a
local-backed pillbox they land at `<state_dir>/repo/`. Either way,
the encryption password stays on the host.

## Forgetting an agent

```sh
pillbox auth rm claude       # wipes ~/.pillbox/global/auth/claude/
pillbox auth login --agent claude         # back to a fresh OAuth flow
```

Useful when you want to log into a different account, or when an agent
is misbehaving and you want a clean slate.

## Rotating a secret

```sh
pillbox secret rm ANTHROPIC_API_KEY
pillbox secret add ANTHROPIC_API_KEY    # paste new value
```

Subsequent `pillbox run --with ANTHROPIC_API_KEY` picks up the
new value automatically — secrets are read at run-time, not at
sandbox-build-time.

## Agent-driven onboarding (the boot-from-zero script)

The flow a coding agent should run when dropped into a new machine
with `pillbox` installed but no state:

```sh
pillbox doctor --json                                # 1. environment sane?
pillbox auth list --json                             # 2. anyone authenticated?
# If not, ask the user to run `pillbox auth login --agent claude` themselves —
# the OAuth browser flow needs a human.

pillbox secret list --json                           # 3. what secrets exist?
pillbox env list --json                              # 4. what bundles exist?
pillbox remote list --json                           # 5. any registered remotes?
pillbox session list --json                          # 6. any detached sessions?
```

From there, build the right `pillbox run` invocation. If a needed
secret / bundle isn't present, ask the user for the value (or where
to source it from) rather than guessing. A non-empty
`pillbox session list` is a signal that something is already
running in the background — surface it before launching another.

## What NOT to do

- ❌ Don't commit `~/.pillbox/` to git — it's plaintext secrets and
  OAuth tokens.
- ❌ Don't `pillbox secret show --reveal --to-stdout > log.txt`. The
  flag exists for piping into another command, not for archival.
- ❌ Don't loop `pillbox secret add` over each line of a `.env`. Use
  `pillbox env load` and `--env BUNDLE`.
- ❌ Don't bind-mount the user's real `~/.ssh` by default. If you need
  SSH inside the sandbox, mount the agent socket per session.
- ❌ Don't assume `pillbox secret add` errors on conflict — it
  overwrites silently unless `--if-not-exists` is set.

## See also

- [secrets.md](./secrets.md) — storage format, idempotency, precedence rules
- [security.md](./security.md) — threat model and reveal-gate detail
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
