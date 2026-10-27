---
id: workspace-snapshots
paths:
  - "src/workspace/**"
  - "src/commands/workspace.rs"
  - "src/commands/bookmark.rs"
  - "src/bookmarks.rs"
entry:
  - "src/commands/workspace.rs"
  - "src/workspace/rustic.rs:repo_opts"
  - "src/workspace/base_cache.rs:materialize_once"
tests:
  - "tests/workspace.rs:push_pull_round_trips_the_workspace"
  - "tests/workspace.rs:bookmark_set_show_and_pull_round_trip"
  - "src/workspace/base_cache.rs:restores_once_under_concurrency"
  - "src/workspace/cow.rs:clone_is_independent"
---

# Workspace snapshots, bookmarks and forks

Users snapshot a project to a local or S3-compatible rustic repository, name a base with a bookmark, and restore/fork it for isolated work. See [configuration](../config.md) and [workspace commands](../commands.md).

## Sub-features

- Push/pull, snapshot metadata/lineage, listing/removal and repository rekey.
- Bookmark names pointing at immutable snapshots.
- Materialize-once base cache, copy-on-write clone and ingest filtering.

## How to get to it (user POV)

In a disposable initialized project, `pillbox push` creates a snapshot; `pillbox snapshot list --json` reports its handle. `pillbox bookmark set base` names the current snapshot. Restore with `pillbox pull` only in a destination where restoring files is intended.

## Driving it with the repository harness

`cargo test --no-default-features --test workspace` performs real local rustic push/pull and bookmark round trips in temporary homes, and checks config errors. `cargo test --no-default-features workspace::base_cache::tests` verifies one restore per immutable key and interrupted-entry recovery. `cargo test --no-default-features workspace::cow::tests` verifies independent contents, modes and symlinks.

## Gotchas

- Local rustic disables its extra cache; S3 uses a per-state-directory cache. The local tests do not demonstrate an R2/S3 network round trip.
- Base-cache keys must represent immutable content; a mutable name cannot safely stand in for a snapshot ID.
- CoW may fall back to a reported byte copy; independent contents are guaranteed, zero-copy performance is filesystem-dependent.
