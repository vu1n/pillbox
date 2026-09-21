# pillbox.toml — the pillbox descriptor

A `pillbox.toml` at a directory's root marks it as a **project
pillbox**. Pillbox walks up from cwd looking for one (like `.gitignore`
or `Cargo.toml`); the first match wins. No match → fall back to the
global pillbox.

For the command-table reference see [../AGENTS.md](../AGENTS.md).

## Schema (v0.6)

```toml
# Required
name = "my-project"

# Optional — run-config defaults for `pillbox run`
agent = "claude"          # "claude" | "codex" | "codex-serve" | "opencode" | "pi" | "cursor"
model = "zai-coding-plan/glm-4.5-air"   # provider/model; omitted → the agent's own default

# Sandbox image. Omitted → the published default (which may not be cached → set this).
[runner]
image = "pillbox-runner:dev"

# Workspace backend. Default is local.
[workspace]
backend = "local"         # or "s3"
# s3-only:
# endpoint = "https://<acct>.r2.cloudflarestorage.com"
# region = "auto"
# bucket = "my-bucket"
# prefix = "pillbox/"
# access_key_env = "R2_ACCESS_KEY"   # env var NAME, not the secret value
# secret_key_env = "R2_SECRET_KEY"

# Named run environments — `pillbox run --preset dev`. See "Presets" below.
[preset.dev]
with = ["ANTHROPIC_API_KEY"]                      # secret NAMES, never values
mcp = ["code-search=http://localhost:8123"]
egress_allow = ["api.anthropic.com", "crates.io", "static.crates.io"]
egress_deny = true
vault = true
```

Unknown top-level fields are rejected with exit 3. The `[workspace]`
table is intentionally permissive so older binaries keep parsing
descriptors that gain fields in future releases — the trade-off is that
typos *inside* `[workspace]` are silently ignored at parse time.

| Field | Type | Notes |
|---|---|---|
| `name` | string | Required. Display name for the pillbox; also defaults `pillbox run`'s `--name`. |
| `agent` | string | Default agent for `pillbox run` (`claude`, `codex`, `codex-serve`, `opencode`, `pi`, or `cursor`). |
| `model` | string | Default model for `pillbox run` (`provider/model`). Omitted → the agent's own default. Overridden by `--model`. |
| `[runner].image` | string | Sandbox image. Omitted → the published default (often uncached). Overridden by `$PILLBOX_RUNNER_IMAGE`. |
| `[workspace].backend` | string | `local` (default) or `s3`. Picks the rustic-backed snapshot store. |
| `[workspace].endpoint` | string | S3-only. URL for R2, MinIO, Backblaze, native S3, etc. |
| `[workspace].region` | string | S3-only. Defaults to `auto`. |
| `[workspace].bucket` | string | S3-only. Bucket name. |
| `[workspace].prefix` | string | S3-only. Object key prefix inside the bucket. |
| `[workspace].access_key_env` | string | S3-only. Env var NAME (not value) that holds the access key. |
| `[workspace].secret_key_env` | string | S3-only. Env var NAME (not value) that holds the secret key. |
| `[preset.NAME].*` | table | A named run environment selected with `pillbox run --preset NAME`. Fields below. |

The S3 credentials are referenced by env-var **name**, not by value, so
`pillbox.toml` stays safe to check into git. Set the env vars in your
shell (or via a secret manager) before `pillbox push` / `pillbox pull`.

### `--from-git` credentials

`pillbox new --from-git URL` shells out to `git clone URL`. If `URL`
embeds a PAT (e.g. `https://ghp_xxx@github.com/...`), git records that
PAT in `<cwd>/.git/config`. Any subsequent `pillbox push` will snapshot
`.git/` and therefore the token. For private repos use a git
credential helper or SSH key — pillbox doesn't strip credentials from
URLs because doing so silently would also break the legitimate "I
know what I'm doing" case.

### Workspace data on disk

Either way, the **encryption password** for the rustic repository
lives at `~/.pillbox/projects/<key>/repo-password` (0600, local-only).
With the S3 backend that means a stolen bucket alone can't be decrypted.

For the local backend, the rustic repo itself lives at
`~/.pillbox/projects/<key>/repo/`.

`pillbox.toml` is the **descriptor** users edit by hand. The durable
record lives in `<state_dir>/meta.json` (see below) and is rewritten by
pillbox.

## State directory and the path key

`pillbox new` creates a state directory under
`~/.pillbox/projects/<key>/`. The key is the **absolute path of the
directory holding `pillbox.toml`, with `/` replaced by `-`**:

```
/Users/vuln/work/myapp          → -Users-vuln-work-myapp
/home/alice/projects/api-svc    → -home-alice-projects-api-svc
```

Greppable, human-readable, unique per machine. Symlinks resolve before
encoding so two paths to the same directory collapse to one key.

```
~/.pillbox/projects/-Users-vuln-work-myapp/
├── meta.json          # { name, created_at, agent_default, workspace }
├── repo-password      # 0600 — rustic repo encryption password (local only)
├── repo/              # local rustic repository (backend = "local")
├── secrets/           # 0700
├── env/               # 0700
├── auth/              # reserved (v0.7 per-project auth override)
├── vault/             # 0700
└── sessions/          # 0700 — detached-session records
```

`meta.json` is rewritten by pillbox; don't edit it directly. To change
the pillbox's name, edit `pillbox.toml`'s `name = ` field and pillbox
will reconcile on the next `pillbox new` (PR 3 will add `pillbox
reconfigure`).

## Discovery rules

- Pillbox starts at `std::env::current_dir()`.
- Walks up looking for `pillbox.toml`.
- Stops at the first match — that descriptor *selects* the pillbox (which secrets/env/state to use).
- Falls back to the global pillbox if nothing is found.

## Run-config cascade

While *pillbox selection* stops at the first descriptor, the **run-config
fields** (`agent`, `model`, `[runner] image`) **cascade** like `CLAUDE.md`:
the selected project `pillbox.toml` is overlaid **field-by-field** on the
user-global defaults at `~/.pillbox/global/pillbox.toml`. Per field,
precedence is:

```
CLI flag  >  env (e.g. $PILLBOX_RUNNER_IMAGE)  >  project pillbox.toml
          >  ~/.pillbox/global/pillbox.toml    >  built-in default
```

So set `agent` / `model` / `[runner] image` **once** in the global file and
every `pillbox run` inherits them — a project descriptor overrides only what's
repo-specific, and unset fields fall through to global. The global file is read
leniently (it's a defaults file, so `name` isn't required and an absent file is
not an error). This is what makes a bare `pillbox run` flagless.

To inspect what discovery resolved to:

```sh
pillbox info          # human
pillbox info --json   # machine
```

## Overriding discovery

```sh
pillbox --pillbox myapp secret list      # operate on named pillbox
pillbox --pillbox global auth list       # explicit global
pillbox --pillbox -Users-vuln-work-myapp info   # by path key
```

`--pillbox` is global — works on every per-pillbox command.

## Presets — a run environment by name

A `[preset.NAME]` table is the set of `pillbox run` flags a project keeps
re-typing, declared once and selected by name:

```sh
pillbox run --preset dev
pillbox run --preset dev --with GH_TOKEN --egress-allow github.com   # extend it
```

The environment then lives in a file that is reviewed and diffed, not in
shell history — which matters most for `egress_allow`, the one list you do
not want widened by whoever is in a hurry. (This is the descriptor-side
version of a Workspace + Gateway resource: authored once, bound by
reference. It is **not** `--profile`, which is the *model* profile handed to
the harness.)

| Field | Same as | Merge with an explicit flag |
|---|---|---|
| `agent` | `--agent` | flag wins |
| `model` | `--model` | flag wins |
| `temperature` | `--temperature` | flag wins |
| `vault` | `--vault` | or-ed (a flag can turn it on, not off) |
| `memory` | `--memory` | or-ed |
| `egress_deny` | `--egress-deny` | or-ed |
| `mount` | `--mount HOST:GUEST` | preset entries first, then the flag's |
| `with` | `--with NAME[=ENV_VAR]` | preset first, then flags |
| `env` | `--env BUNDLE` | preset first, then flags |
| `env_file` | `--env-file PATH` | preset first, then flags; a relative path resolves against the directory of the descriptor that declared it |
| `mcp` | `--mcp NAME=URL` | preset first, then flags |
| `mcp_token` | `--mcp-token NAME=SECRET_NAME` | preset first, then flags |
| `egress_allow` | `--egress-allow HOST` | preset first, then flags |

Rules:

- Every value is a **name** or a non-secret setting. `with` names a secret in
  the store; a preset never holds a value, so it is safe to commit.
- A preset is strict: an unknown field (`mounts = …`) fails at load with exit 3,
  before any VM boots. `--preset NAME` for a name that isn't declared also
  fails with exit 3 and lists the presets that are.
- Presets **cascade by name**: a `[preset.dev]` in the project descriptor
  replaces a global `[preset.dev]` whole (no field-wise merge — a preset is one
  reviewable unit); global presets the project does not redeclare stay
  selectable.
- The applied preset prints one stderr line before the run
  (`pillbox: preset \`dev\`: 1 secret(s), vault, egress default-deny [...]`) so
  a mis-targeted run is visible before the agent reaches the wrong endpoint.
- Precedence overall: `CLI flag > --preset > project pillbox.toml >
  ~/.pillbox/global/pillbox.toml > built-in default`.

Presets replace the v0.5 top-level run defaults (`with = [...]`, `mount`,
`env`, `env_file`) that v0.6 dropped for sprawling the descriptor. Those
top-level keys are still rejected; the same lists now live under a **name**,
which is what keeps them from hiding behavior — a bare `pillbox run` applies no
preset.

## Anti-patterns

- Don't put secret values in `pillbox.toml`. Plaintext config, often
  committed. Use `pillbox secret add` and reference by name via
  `--with`.
- Don't expect multiple configs to merge. One is found and used.
- Don't edit `meta.json` directly. Edit the descriptor.

## See also

- [secrets.md](./secrets.md) — pillbox-scoped secrets + env bundles
- [vault.md](./vault.md) — per-pillbox vault state
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
