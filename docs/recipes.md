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

# Authenticate
pillbox claude login        # OAuth in browser
pillbox codex login         # device code (URL + code printed)

# Confirm
pillbox auth list
```

## One agent, one API key

The simplest possible flow.

```sh
pillbox secret add ANTHROPIC_API_KEY              # paste, then Ctrl-D
pillbox claude run --with ANTHROPIC_API_KEY
```

Inside the sandbox, claude sees `$ANTHROPIC_API_KEY` set to the value
you pasted. cwd is mounted at `/workspace/<basename(cwd)>`.

## Targeting dev / staging / prod

Load three bundles once, pick one per run.

```sh
pillbox env load dev   ~/work/myapp/.env.dev
pillbox env load stage ~/work/myapp/.env.staging
pillbox env load prod  ~/work/myapp/.env.prod

pillbox claude run --env dev
pillbox claude run --env stage
pillbox claude run --env prod
```

To override a single variable for one run (e.g. swap in your personal
API key):

```sh
pillbox claude run --env prod --with ANTHROPIC_API_KEY
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
pillbox claude run --mount ~/.aws:/home/lum/.aws:ro

# Sibling repo at /workspace/sibling (in addition to cwd)
pillbox claude run --mount ~/work/sibling-repo:/workspace/sibling

# SSH agent socket (Linux)
pillbox claude run --mount $SSH_AUTH_SOCK:/ssh-agent --with SSH_AUTH_SOCK=SSH_AUTH_SOCK
```

The agent's working directory inside the sandbox is always
`/workspace/<name>` where `name` defaults to `basename(cwd)` (override
with `--name`).

## Multi-repo workspace

```sh
cd ~/work/primary
pillbox claude run \
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

## Forgetting an agent

```sh
pillbox auth rm claude       # wipes ~/.pillbox/data/claude/
pillbox claude login         # back to a fresh OAuth flow
```

Useful when you want to log into a different account, or when an agent
is misbehaving and you want a clean slate.

## Rotating a secret

```sh
pillbox secret rm ANTHROPIC_API_KEY
pillbox secret add ANTHROPIC_API_KEY    # paste new value
```

Subsequent `pillbox claude run --with ANTHROPIC_API_KEY` picks up the
new value automatically — secrets are read at run-time, not at
sandbox-build-time.

## Agent-driven onboarding (the boot-from-zero script)

The flow a coding agent should run when dropped into a new machine
with `pillbox` installed but no state:

```sh
pillbox doctor --json                                # 1. environment sane?
pillbox auth list --json                             # 2. anyone authenticated?
# If not, ask the user to run `pillbox claude login` themselves —
# the OAuth browser flow needs a human.

pillbox secret list --json                           # 3. what secrets exist?
pillbox env list --json                              # 4. what bundles exist?
```

From there, build the right `pillbox claude run` invocation. If a
needed secret/bundle isn't present, ask the user for the value (or
where to source it from) rather than guessing.

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
