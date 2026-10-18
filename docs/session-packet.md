# Session packet (`pillbox.session-packet/1`)

A session packet carries one session's workspace and its harness's state
between turns, on every placement (Cloudflare container, local container
runner, local microVM). It replaces Stratus's ad hoc
`tar cf --exclude=.credentials.json workspace/repo root/.claude/projects` plus
a side metadata JSON. The engine is its only writer and reader:

```sh
pillbox checkpoint pack   --workspace DIR [--harness-state DIR] --harness NAME \
                          [--harness-session-id ID] [--transcript REL] --out FILE [--json]
pillbox checkpoint unpack --packet FILE --expect sha256:<hex> --workspace DIR \
                          [--harness-state DIR] [--allow-nonempty] [--json]
```

Both are hidden engine subcommands (see [commands.md](./commands.md#engine-subcommands-stratus))
and are in the `--no-default-features` Linux build. Code: `src/checkpoint/`.

`--out` may not be inside `--workspace` or `--harness-state` (exit 2), since the next pack
would pack the old packet. Unpack sets every directory it creates to 0755 whatever the umask.

## Specification

The manifest is specified by the JSON Schema
[`proto/pillbox/session-packet-1.schema.json`](../proto/pillbox/session-packet-1.schema.json);
the Rust types follow it. Every object is closed (`additionalProperties: false`)
and an absent optional is omitted, never `null`.

### Tar layout

One POSIX ustar archive. A pax `x` header (fixed name `././@PaxHeader`, keys
`path` and `linkpath` only) appears only before an entry whose path or link
target does not fit ustar's fields.

1. `packet.json` first: the manifest, compact JSON exactly as `pack` serializes it.
2. Then every manifest entry, in manifest order (byte-sorted paths), under
   `workspace/` (the `--workspace` tree) and `harness/` (the `--harness-state`
   tree). The tree roots themselves are not entries.

Every header has mtime 0, uid/gid 0, empty user and group names, and mode 0644
(file), 0755 (executable file or directory) or 0777 (symlink). Only regular
files, directories and symlinks appear: no hard links, devices, FIFOs or
sockets. The archive ends with two zero blocks and nothing after them. The same
trees therefore always give the same bytes.

**Digest:** `sha256:` + lowercase hex SHA-256 of the whole tar file.

### Manifest

| Field | |
|---|---|
| `format` | `"pillbox.session-packet/1"` |
| `harness` | harness id, `^[a-z][a-z0-9_]{0,63}$`, e.g. `claude_code` |
| `harness_session_id` | optional; the harness's own session id |
| `transcript` | optional; transcript path relative to `harness/`, must name a file entry |
| `entries` | `{kind:"dir",path}`, `{kind:"file",path,size,sha256,executable}`, `{kind:"symlink",path,target}` |

Entry rules (`checkpoint::validate`, run by both `pack` and `unpack`): paths
start with `workspace/` or `harness/`, have no empty, `.` or `..` component, no
NUL, at most 4096 bytes; entries are sorted and unique; each entry's parent is
a tree root or an earlier `dir` entry (so nothing is written through a symlink).
A symlink target is relative and resolves lexically inside the link's own tree
without passing through another symlink in the packet; `pack` refuses any other
link instead of dropping it.

## Exclusions

Never packed from either tree (reported in `excluded` with reason `credential`):

- `.credentials.json` (Claude OAuth), `pillbox-vault-ca.crt`, `pillbox-vault-ca.key`
  (the vault CA).

Also never packed from the `harness/` tree: the basename of every agent's
credential sentinel in `src/agents/mod.rs` (`ALL[*].cred_sentinel`), today
`.credentials.json`, `auth.json` (codex, cursor, pi, prime-agent) and
`opencode.db`. In the workspace tree those names are ordinary source files and
are kept.

Sockets, FIFOs and devices are skipped with reason `special`. The workspace
ingest denylist (`.env*`, `*.pem`, `.ssh/`, …; `src/workspace/ingest.rs`) is
**not** applied: it governs what enters a sandbox, and a packet must restore
the workspace the agent left, including checked-in key fixtures.

## Limits

| Limit | Value |
|---|---|
| Packet size (`pack` writes, `unpack` reads) | 1 GiB |
| Entries | 250,000 |
| `packet.json` size on unpack | 64 MiB |
| Path, link target | 4096 bytes |

Past a limit, `pack` writes nothing and `unpack` refuses (exit 1).

## Unpack checks

In order, with nothing written until the last one passes for the step that
writes:

1. `--expect` is well formed (exit 2 otherwise) and equals the digest of the
   file (exit 1 otherwise).
2. `packet.json` is the first entry, parses, is byte-identical to its canonical
   serialization (so `null`, unknown fields or reordering are refused), and
   passes the entry rules above.
3. Targets: created if missing; a non-empty target is refused (exit 2) unless
   `--allow-nonempty`. A packet with `harness/` entries needs `--harness-state`.
4. Each tar entry must equal the next manifest entry (path, kind, size,
   executable, link target); each file's bytes must match its `sha256`. Entries
   are written into a staging directory inside each target.
5. The file is hashed again while read; if it changed, nothing is moved.
6. Staged entries move into the targets. With `--allow-nonempty`, packet paths
   replace what is there and other files are kept.

## JSON output

```jsonc
// pillbox checkpoint pack --json
{ "version": 1, "format": "pillbox.session-packet/1",
  "digest": "sha256:…", "bytes": 10240,
  "manifest": { … packet.json … },
  "excluded": [ { "path": "harness/.credentials.json", "reason": "credential" } ] }

// pillbox checkpoint unpack --json
{ "version": 1, "format": "pillbox.session-packet/1",
  "digest": "sha256:…", "manifest": { … } }
```
