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

# Optional — default agent for `pillbox run`
agent = "claude"          # "claude" | "codex" | "codex-serve" | "opencode" | "pi"

# Workspace backend (PR 3). Default is local.
[workspace]
backend = "local"         # or "s3"
# s3-only:
# endpoint = "https://<acct>.r2.cloudflarestorage.com"
# region = "auto"
# bucket = "my-bucket"
# prefix = "pillbox/"
# access_key_env = "R2_ACCESS_KEY"   # env var NAME, not the secret value
# secret_key_env = "R2_SECRET_KEY"
```

Unknown top-level fields are rejected with exit 3. The `[workspace]`
table is intentionally permissive so older binaries keep parsing
descriptors that gain fields in future releases — the trade-off is that
typos *inside* `[workspace]` are silently ignored at parse time.

| Field | Type | Notes |
|---|---|---|
| `name` | string | Required. Display name for the pillbox; also defaults `pillbox run`'s `--name`. |
| `agent` | string | Default agent for `pillbox run` (`claude`, `codex`, `codex-serve`, `opencode`, or `pi`). |
| `[workspace].backend` | string | `local` (default) or `s3`. Picks the rustic-backed snapshot store. |
| `[workspace].endpoint` | string | S3-only. URL for R2, MinIO, Backblaze, native S3, etc. |
| `[workspace].region` | string | S3-only. Defaults to `auto`. |
| `[workspace].bucket` | string | S3-only. Bucket name. |
| `[workspace].prefix` | string | S3-only. Object key prefix inside the bucket. |
| `[workspace].access_key_env` | string | S3-only. Env var NAME (not value) that holds the access key. |
| `[workspace].secret_key_env` | string | S3-only. Env var NAME (not value) that holds the secret key. |

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
- Stops at the first match (multiple configs in the path are NOT merged).
- Falls back to the global pillbox if nothing is found.

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

## What's NOT in pillbox.toml (v0.6 PR 2)

v0.5 had multi-value defaults (`with`, `mount`, `env_file`, `env`) for
the `run` flags. v0.6 drops them — they sprawled the descriptor and
hid behavior. CLI flags are the single source of truth for those.
Re-add by alias if you need:

```sh
# Old v0.5: pillbox.toml had `with = ["ANTHROPIC_API_KEY"]`
# New v0.6: pass at the command line, or wrap in a shell alias.
alias pbrun='pillbox run --with ANTHROPIC_API_KEY'
```

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
