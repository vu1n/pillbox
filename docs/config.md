# Per-project config (`pillbox.toml`)

A `pillbox.toml` at the project root supplies defaults for `pillbox
<agent> run` flags. Pillbox walks up from the current working directory
to find one, like `.gitignore` or `Cargo.toml`. The first file found
wins.

For the command-table reference see [../AGENTS.md](../AGENTS.md).

## Quick example

```toml
# ~/work/myapp/pillbox.toml
name = "myapp"
env = "dev"
with = ["ANTHROPIC_API_KEY"]
mount = ["~/.aws:/home/lum/.aws:ro"]
env_file = [".env.local"]
```

```sh
cd ~/work/myapp
pillbox claude run           # uses the four defaults above
pillbox claude run --env stage   # appends 'stage' to ['dev']; both layered
```

## Schema

All fields optional. Unknown fields are rejected with exit 3.

| Field | Type | Maps to | Notes |
|---|---|---|---|
| `name` | string | `--name` | Single-value: CLI overrides. |
| `env` | string | `--env BUNDLE` | Single-value, but applied as the first env layer at run-time. Lowest precedence in env composition. |
| `with` | string list | `--with NAME[=ENV_VAR]` | CLI entries appended. |
| `mount` | string list | `--mount HOST:GUEST[:opts]` | CLI entries appended. Tilde-expanded. |
| `env_file` | string list | `--env-file PATH` | CLI entries appended. Tilde-expanded. Paths resolved relative to cwd at invocation, NOT relative to the config file. |

## Merge rules

- **Single-value fields** (`name`, `env`): CLI flag overrides the file's value.
- **Multi-value fields** (`with`, `mount`, `env_file`): file's list comes first, CLI list is appended. Both apply.

The env composition order at run-time is unchanged:

```
config env  →  cli --env  →  config env_file  →  cli --env-file  →  config with  →  cli --with
        (lowest)                                                                          (highest)
```

If two layers set the same key, pillbox emits one line to stderr per shadowed variable (see [secrets.md](./secrets.md) for the exact formats).

## Discovery rules

- Pillbox starts at `std::env::current_dir()`.
- It walks up parent directories until it finds `pillbox.toml`.
- It stops at the first match — multiple configs in the path are NOT merged.
- It stops at the filesystem root with no match.

To inspect what pillbox found (or didn't):

```sh
pillbox config           # human-readable
pillbox config --json    # for scripts; includes "source" path
```

## Escape hatches

```sh
pillbox claude run --no-config              # skip discovery entirely
pillbox claude run --config /tmp/other.toml # use a specific file
```

`--config` and `--no-config` are mutually exclusive.

## Tilde expansion

Paths in `mount` and `env_file` are expanded against `$HOME` when they
start with `~/`. CLI flags don't need this — the shell already expanded
`~` before pillbox saw the argument.

`mount = ["~/.aws:/home/lum/.aws:ro"]` becomes
`/Users/you/.aws:/home/lum/.aws:ro` before being passed to Docker.

## Anti-patterns

- ❌ **Don't put secret values in `pillbox.toml`.** It's plaintext config,
  often checked into git. Use `pillbox secret add NAME` and reference by
  name via `with = ["NAME"]`.
- ❌ Don't expect multiple configs to merge. Pillbox finds one and uses
  it. If you want shared config across projects, symlink or factor it
  out yourself.
- ❌ Don't put `env_file` paths relative to the config file location.
  They're resolved against cwd at invocation, not the config's directory.

## See also

- [secrets.md](./secrets.md) — what `with` references and how composition works
- [recipes.md](./recipes.md) — copy-paste flows including `pillbox.toml` setups
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference
