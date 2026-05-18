# Secrets and env bundles

For the command reference, see [AGENTS.md](../AGENTS.md). This doc covers
storage, lifecycle, and the precedence rules in detail.

## Two flavors, same posture

| | secret | env bundle |
|---|---|---|
| **Subject** | `pillbox secret …` | `pillbox env …` |
| **Holds** | one value | many KEY=VALUE pairs |
| **Source** | stdin or `--from-env VAR` | a `.env`-formatted file |
| **Storage** | `~/.pillbox/secrets/<name>` (0600) | `~/.pillbox/env/<name>` (0600) |
| **Inject at run** | `--with NAME[=ENV_VAR]` | `--env BUNDLE` |
| **Use when** | one credential | shipping dev/staging/prod as a unit |

Both live under `~/.pillbox/` (parent dir 0700, files 0600 — same posture
as `~/.aws/credentials`).

## Naming rules

ASCII alphanumeric plus `_`, `-`, `.`. No path separators, no spaces, no
empty names. The same rule applies to secrets and bundles; both share
`paths::validate_name`, so what works in one works in the other.

```sh
pillbox secret add ANTHROPIC_API_KEY    # ok
pillbox secret add db.staging           # ok
pillbox secret add foo-bar              # ok
pillbox secret add ../etc/passwd        # rejected (exit 2)
pillbox secret add "foo bar"            # rejected (exit 2)
```

## Idempotency model

Both `secret add` and `env load` **overwrite silently by default**. This
is the right default for setup scripts that re-run on every boot.

If you want create-only semantics, pass `--if-not-exists` — that errors
with exit 1 when the name is already taken:

```sh
pillbox secret add ANTHROPIC_API_KEY --if-not-exists < /dev/null
# pillbox: secret add failed. `ANTHROPIC_API_KEY` already exists.
#   Next: pillbox secret rm ANTHROPIC_API_KEY  # then re-add  (or drop --if-not-exists)
```

`rm` on a missing name is a no-op, exit 0 — also script-friendly.

## Reading values back

`secret show` and `env show` mask by default (last 4 chars visible, the
rest replaced with `*`). To get the plain value, pass `--reveal`.
Pillbox refuses to write the unmasked value to a non-TTY stdout unless
you also pass `--to-stdout`:

```sh
pillbox secret show ANTHROPIC_API_KEY                     # sk-ant-***...abcd
pillbox secret show ANTHROPIC_API_KEY --reveal            # full value, only if TTY
echo $(pillbox secret show … --reveal)                    # refuses, exit 2
echo $(pillbox secret show … --reveal --to-stdout)        # full value, you asked
```

The `--to-stdout` gate exists to make leaking secrets into log files or
CI captures a deliberate act, not an accident.

## `--with` and the env composition order

`--with NAME` binds the stored secret to `NAME` in the guest env.
`--with NAME=ENV_VAR` injects the secret stored under `NAME` as
`ENV_VAR` instead. Useful when an agent expects `OPENAI_API_KEY` but
you've named the stored secret `openai_personal`:

```sh
pillbox secret add openai_personal --from-env OPENAI_API_KEY
pillbox claude run --with openai_personal=OPENAI_API_KEY
```

When `--env`, `--env-file`, and `--with` all touch the same KEY,
precedence is **lowest to highest**:

```
--env BUNDLE   (lowest — whole stored bundle)
--env-file PATH (one-off file)
--with NAME    (single secret — wins)
```

Pillbox prints one line to stderr per shadowed variable so the override
is visible. The exact format depends on which layer wins:

```
pillbox: note: ANTHROPIC_API_KEY shadowed by --env prod (was set to `***xxxx`)
pillbox: note: ANTHROPIC_API_KEY shadowed by --env-file ./extra.env
pillbox: note: ANTHROPIC_API_KEY shadowed by --with ANTHROPIC_API_KEY
```

The order is deliberate: shipping a whole environment with `--env prod`
is the common case; you reach for `--with` when you want to override one
slot of that environment for a single run.

## `.env` parser limitations

`pillbox env load` parses a deliberate subset of the `.env` format:

- One `KEY=VALUE` per line. Leading whitespace allowed.
- `#` starts a line comment (after optional whitespace).
- Optional leading `export ` is stripped.
- Single or double quotes around the value are stripped (one pair).
- Keys must match `[A-Z_][A-Z0-9_]*` (any case).

**Not** supported:

- Variable interpolation (`FOO=$BAR`) — stored literally.
- Command substitution (`FOO=$(date)`) — stored literally.
- Multi-line values.
- Escape sequences (`\n`, `\"`, …).

If you need any of those, run the file through `envsubst` or `set -a;
source file; set +a` first, then `pillbox env load` the result.

## Lifecycle: a real one

```sh
# Initial setup
pillbox secret add ANTHROPIC_API_KEY                      # paste, Ctrl-D
pillbox env load prod  ~/work/myapp/.env.prod
pillbox env load stage ~/work/myapp/.env.staging

# Routine use
pillbox claude run --env stage
pillbox claude run --env prod --with ANTHROPIC_API_KEY    # override

# Rotation
pillbox secret rm ANTHROPIC_API_KEY
pillbox secret add ANTHROPIC_API_KEY                      # paste new value

# Audit (machine-readable)
pillbox secret list --json
pillbox env list --json
pillbox env show prod --json                              # masked
```

## Bulk import from an existing `.env`

If you already have a `.env` with everything in it, load it as a bundle
and use `--env`. Don't loop `secret add` over each key — bundles exist
for exactly this.

```sh
pillbox env load all ~/work/myapp/.env
pillbox claude run --env all
```

If a single high-value credential needs `--with`-grade precedence (e.g.
you want to shadow it on some runs), break it out:

```sh
pillbox secret add ANTHROPIC_API_KEY --from-env ANTHROPIC_API_KEY
pillbox claude run --env all --with ANTHROPIC_API_KEY
```

## Vaulted secrets

`pillbox secret add NAME --vault` marks a secret for stub-swap at
injection time. With the flag set, `--with NAME` injects a stub value
into the guest env instead of the real secret; the MITM proxy swaps
stub → real on egress to the secret's host. A leaked stub from inside
the sandbox is useless to an attacker — it only resolves to the real
key while the run's vault session is alive.

```sh
# Known names: host + scheme + prefix come from the built-in registry.
pillbox secret add ANTHROPIC_API_KEY --vault            # api.anthropic.com / x-api-key
pillbox secret add OPENAI_API_KEY    --vault            # api.openai.com    / Authorization: Bearer
pillbox secret add GITHUB_TOKEN      --vault            # api.github.com    / Authorization: Bearer
pillbox secret add GH_TOKEN          --vault            # alias for GITHUB_TOKEN

# Custom name → known mapping:
pillbox secret add MY_ANTHROPIC --vault --maps-to ANTHROPIC_API_KEY

# Custom name → fully specified:
pillbox secret add INTERNAL --vault \
    --host api.internal.example.com \
    --header-scheme x-api-key \
    --prefix int-
```

At run time the existing `--with NAME` automatically uses the stub if
the secret has a `.meta.json` sidecar. Storage:

```
~/.pillbox/secrets/
├── ANTHROPIC_API_KEY            # 0600, real value
├── ANTHROPIC_API_KEY.meta.json  # 0600, { vault: {host, header_scheme, prefix} }
└── PLAIN_SECRET                 # 0600, real value (no sidecar = not vaulted)
```

`pillbox secret rm NAME` removes both files. Re-adding without
`--vault` cleans up any stale sidecar. See [vault.md](./vault.md) for
the proxy + provider architecture.

## What pillbox does NOT do

- **Encrypt at rest.** Files are 0600 plaintext. Disk encryption
  (FileVault / LUKS / BitLocker) is the at-rest defense.
- **Vault the value from the agent on bare `secret add`.** Plain
  (non-`--vault`) `--with` injects the real value. Use `--vault` at add
  time for stub-swap behavior.
- **Vault non-HTTP-API secrets.** The proxy only sees HTTPS to known
  hosts. SSH keys, database passwords, etc. fall back to plain mount.
- **Sync across machines.** One secret store per OS user, per host.

## See also

- [recipes.md](./recipes.md) — copy-paste flows for the common patterns above
- [security.md](./security.md) — full threat model + reveal-gate rationale
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
