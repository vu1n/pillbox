# Secrets and env bundles

For the command reference, see [AGENTS.md](../AGENTS.md). This doc
covers storage, lifecycle, inheritance, and the precedence rules in
detail.

## Pillbox-scoped (v0.6 model)

Every secret and env bundle is scoped to a pillbox. A pillbox is the
global pillbox at `~/.pillbox/global/` or a project pillbox at
`~/.pillbox/projects/<key>/`. Secrets live under
`<pillbox>/secrets/<name>` and env bundles under
`<pillbox>/env/<name>` (all 0600).

Reads merge global + project. Writes default to the resolved pillbox
(or the global pillbox with `--global`).

## Two flavors, same posture

| | secret | env bundle |
|---|---|---|
| **Holds** | one value | many KEY=VALUE pairs |
| **Source** | stdin or `--from-env VAR` | a `.env`-formatted file |
| **Storage** | `<pillbox>/secrets/<name>` (0600) | `<pillbox>/env/<name>` (0600) |
| **Inject at run** | `--with NAME[=ENV_VAR]` | `--env BUNDLE` |
| **Use when** | one credential | shipping dev/staging/prod as a unit |

Parent dirs are 0700. Same posture as `~/.aws/credentials`.

## Inheritance

Inside a project pillbox, reads walk the chain `project → global`. The
first scope with the name wins. So:

- Secret `OPENAI_API_KEY` exists only in global → project reads see
  the global value.
- Secret `OPENAI_API_KEY` exists in both → project value wins (global
  is shadowed).
- Secret `OPENAI_API_KEY` exists only in project → global doesn't see
  it.

`pillbox secret list` annotates each entry with which scope provided
it so the layering is visible.

```sh
cd ~/work/myapp
pillbox secret list
# Secrets visible from `myapp` (project shadows global on conflict):
#   ANTHROPIC_API_KEY  [global]
#   STAGING_DB_URL     [project]
```

Env bundles use the same rules. Bundles are atomic — one full file
wins; we don't merge KV pairs across scopes.

## Choosing a scope at write time

```sh
# Defaults to the resolved pillbox.
pillbox secret add MY_KEY                  # project, when inside one
pillbox secret add MY_KEY                  # global, when not

# Force the global pillbox from anywhere.
pillbox secret add SHARED_KEY --global

# Operate on a specific pillbox by name.
pillbox --pillbox myapp secret add KEY     # project "myapp"
pillbox --pillbox global secret list
```

## Naming rules

ASCII alphanumeric plus `_`, `-`, `.`. No path separators, no spaces,
no empty names. The same rule applies to secrets and bundles.

## Idempotency model

Both `secret add` and `env load` **overwrite silently by default**.
This is the right default for setup scripts that re-run on every boot.

If you want create-only semantics, pass `--if-not-exists` — that errors
with exit 1 when the name is already taken in the **chosen** scope.
Inherited names in a different scope don't block (that's the point of
layering).

```sh
pillbox secret add ANTHROPIC_API_KEY --if-not-exists < /dev/null
# pillbox: secret add failed. `ANTHROPIC_API_KEY` already exists in `myapp`.
#   Next: pillbox secret rm ANTHROPIC_API_KEY  # then re-add  (or drop --if-not-exists)
```

`rm` on a missing name is a no-op, exit 0.

## Reading values back

`secret show` and `env show` mask by default (last 4 chars visible).
`--reveal` unmasks, but only to a TTY unless `--to-stdout` is also
passed. Same posture as v0.5.

`secret show` notes the source scope so you can see what the
inheritance resolved to:

```
ANTHROPIC_API_KEY=sk-ant-***************abcd  [from global]
```

## `--with` and the env composition order

`--with NAME` binds the stored secret to `NAME` in the guest env.
`--with NAME=ENV_VAR` injects the secret stored under `NAME` as
`ENV_VAR`. Useful when an agent expects `OPENAI_API_KEY` but you've
named the stored secret `openai_personal`.

```sh
pillbox secret add openai_personal --from-env OPENAI_API_KEY
pillbox run --with openai_personal=OPENAI_API_KEY
```

When `--env`, `--env-file`, and `--with` all touch the same KEY,
precedence is **lowest to highest**:

```
--env BUNDLE   (lowest — whole stored bundle)
--env-file PATH
--with NAME    (highest — single secret wins)
```

Pillbox emits one stderr line per shadowed variable so the override is
visible.

## `.env` parser limitations

`pillbox env load` parses a deliberate subset:

- One `KEY=VALUE` per line. Leading whitespace allowed.
- `#` starts a comment.
- Optional leading `export ` is stripped.
- Single or double quotes around the value are stripped (one pair).
- Keys: `[A-Z_][A-Z0-9_]*` (any case).

Not supported: interpolation, command substitution, multi-line values,
escape sequences.

## Vaulted secrets

`pillbox secret add NAME --vault` marks a secret for stub-swap at
injection time. With the flag set, `--with NAME` injects a stub value
into the guest env instead of the real secret; the MITM proxy swaps
stub → real on egress to the secret's host. A leaked stub from inside
the sandbox is useless to an attacker.

```sh
# Known names — host / scheme / prefix from the built-in registry.
pillbox secret add ANTHROPIC_API_KEY --vault            # api.anthropic.com / x-api-key
pillbox secret add OPENAI_API_KEY    --vault            # api.openai.com    / Authorization: Bearer
pillbox secret add GITHUB_TOKEN      --vault            # api.github.com    / Authorization: Bearer

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
<pillbox>/secrets/
├── ANTHROPIC_API_KEY            # 0600, real value
├── ANTHROPIC_API_KEY.meta.json  # 0600, { vault: {host, header_scheme, prefix} }
└── PLAIN_SECRET                 # 0600 — no sidecar, not vaulted
```

`pillbox secret rm NAME` removes both files. Re-adding without
`--vault` cleans up any stale sidecar. See [vault.md](./vault.md) for
the proxy architecture.

## What pillbox does NOT do

- **Encrypt at rest.** Files are 0600 plaintext. Disk encryption is the
  at-rest defense.
- **Vault the value from the agent on bare `secret add`.** Plain (non-
  `--vault`) `--with` injects the real value.
- **Vault non-HTTP secrets.** Only HTTPS to known hosts.
- **Sync across machines.** One store per OS user, per host.

## See also

- [config.md](./config.md) — pillbox.toml descriptor
- [vault.md](./vault.md) — per-pillbox credential vault
- [security.md](./security.md) — threat model
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
