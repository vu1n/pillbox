# Mesa workspace/forking review

> **Archived ADR (2026-05-30).** Decision ("keep rustic; don't adopt Mesa for
> the remote story") still holds and is now absorbed into
> [remotes-redesign.md](./remotes-redesign.md) — see its "Rustic isn't going
> away — it was doing two jobs" section. Kept for the record; not active docs.

Reviewed: 2026-05-26

Mesa docs reviewed:

- https://docs.mesa.dev/content/getting-started/introduction
- https://docs.mesa.dev/content/getting-started/quickstart
- https://docs.mesa.dev/content/core-concepts/versioning
- https://docs.mesa.dev/content/core-concepts/virtual-filesystem
- https://docs.mesa.dev/content/core-concepts/patterns-best-practices
- https://docs.mesa.dev/content/mesafs/application-mount
- https://docs.mesa.dev/content/mesafs/cli-mount
- https://docs.mesa.dev/content/git-server/overview
- https://docs.mesa.dev/content/getting-started/auth-and-permissions
- https://mesa.dev/pricing

## Short answer

Mesa's concrete advantage over pillbox's current rustic implementation is
diff/merge/bookmark workflow and a hosted virtual filesystem, not persistence,
forking, or lineage by themselves.

Pillbox already has the important primitives in code: encrypted rustic
snapshots, S3-compatible storage, snapshot bookmarks, session `base_snapshot`,
session `result_snapshot`, `session pull`, and `--parent` trace metadata. The
remote handoff now carries S3/R2 workspace config, credentials, repo password,
and base snapshot material so remote runners can hydrate before the agent starts
and push a result afterward.

Recommendation: do not replace rustic with Mesa for the remote story. Keep Mesa
as a possible future backend only if pillbox wants native diff/merge/review
workflows or a hosted VFS rather than just durable remote workspace state.

## Fit against pillbox today

Current pillbox workspace state from the codebase:

- `src/workspace/mod.rs` exposes a snapshot-shaped `WorkspaceBackend`.
- `src/workspace/rustic.rs` stores encrypted whole-workspace snapshots locally
  or in S3-compatible storage via rustic's OpenDAL S3 backend.
- `src/config.rs` has `[workspace] backend = "local" | "s3"` plus endpoint,
  region, bucket, prefix, and credential env var fields.
- `src/pillbox.rs` turns that workspace config into a `RusticBackend`.
- `src/bookmarks.rs` stores named, movable refs to immutable snapshot handles.
- `src/session.rs` records `base_snapshot` and `result_snapshot`.
- `src/commands/session.rs` can pull a completed session's result snapshot.
- `src/main.rs` has `--parent` for fork trace metadata.
- `src/sandbox/remote_ssh.rs` and `src/sandbox/remote_e2b.rs` require an
  S3-shaped workspace for remote runs; the remote blob carries the workspace
  material needed to hydrate/push against that repo.

Mesa's model:

- Repository: isolated versioned folder with permissions.
- Change: logical snapshot/commit.
- Bookmark: lightweight movable pointer similar to a branch.
- First write through a mount forks a new change from the mounted base, leaving
  the mounted bookmark unchanged until explicitly moved.
- Recommended agent workflow is repo-per-project, timeline-per-session, and
  proposal bookmarks plus diffs for approval.

This is a useful conceptual match for pillbox, but not because pillbox lacks
fork lineage. The meaningful gap is that rustic snapshots are immutable
checkpoints and intentionally do not provide branch, merge, diff, or conflict
concepts.

## Suggested mapping

| Pillbox concept | Mesa concept |
|---|---|
| Project pillbox workspace | Mesa repository |
| Promoted workspace state | `main` bookmark |
| `pillbox push` snapshot | Mesa change |
| `Session.base_snapshot` | Base Mesa change id |
| `Session.result_snapshot` | Result Mesa change id or session bookmark head |
| Detached/forked run | `session/<session-id>` or `proposal/<run-id>` bookmark |
| `pillbox session pull` | Materialize result change/bookmark via MesaFS or Git clone/fetch |
| Human approval | Diff proposal bookmark vs `main`, then merge bookmark into `main` |

The cleanest session flow would be:

1. Create or reuse one Mesa repository per project pillbox.
2. At `pillbox run --remote --detach`, create `session/<id>` at `main` or at
   the requested parent/base change.
3. Start the sandbox with MesaFS mounted at `/workspace/<name>` on that session
   bookmark.
4. Agent writes persist to Mesa as a new change/fork.
5. On completion, record the current Mesa change id as `result_snapshot`.
6. `session pull` materializes that change into a local directory.
7. Approval merges `session/<id>` or `proposal/<id>` into `main`; rejection
   deletes the bookmark.

## Rustic remote protocol

The existing S3/R2 rustic backend is the durable remote-workspace transport.

Current shape visible in code:

- The remote launch blob carries secrets, env, vault settings, agent args,
  mount name, workspace backend config, repository password, and base snapshot.
- Without `--from-bookmark`, the host snapshots cwd immediately before remote
  startup and uses that snapshot as the base.
- With `--from-bookmark`, the host resolves the bookmark and uses that snapshot
  as the base.
- The remote side creates an isolated temp workspace, pulls the base snapshot
  into it, runs Docker against that directory, then pushes a result snapshot.
- E2B reads the result handle from `PILLBOX_RESULT_SNAPSHOT_FILE` and passes it
  through to `session done --result-snapshot`.

Detached E2B sessions still need the configured webhook/orchestrator path if
the host-side registry must be updated after the local helper has exited.

## Mesa integration shape

If Mesa is still worth exploring, add it as a new workspace backend rather than
changing the rustic backend in place:

```toml
[workspace]
backend = "mesa"
org = "my-org"
repo = "my-project"
default_bookmark = "main"
api_key_env = "MESA_API_KEY"
```

Likely Mesa implementation path:

1. Build a small Mesa smoke runner outside the CLI first: create repo, create
   session bookmark, mount or clone, write files, get diff, merge, delete
   bookmark.
2. Add config parsing for `backend = "mesa"` while leaving `local` and `s3`
   unchanged.
3. Decide whether the first implementation uses Git transport or MesaFS:
   - Git transport is simpler and avoids FUSE, but gives up some MesaFS
     advantages and still requires materializing the repo.
   - MesaFS is the better target for remote sandboxes and large workspaces, but
     needs FUSE and CLI/template changes.
4. Extend the workspace abstraction. The current trait only has push/pull/list
   snapshots; for Mesa we need explicit fork/diff/merge/bookmark operations.
5. Wire E2B and SSH remote launch to mount or clone the Mesa repo before
   starting the agent.

## Why not a direct replacement yet

- Hosted dependency: default Mesa runs at `app.mesa.dev`; code and file history
  leave the user's infrastructure unless they are on enterprise/on-prem.
- Credentials: we need a new secret path for Mesa API keys and probably
  repo-scoped short-lived keys for sandboxes.
- FUSE dependency: full coding agents need native paths, dependency installs,
  compilers, and language servers. Mesa's app-level just-bash path is useful
  for lightweight tool-calling agents, but not enough for pillbox's Docker/E2B
  coding-agent model.
- Platform work: runner images and E2B templates would need `mesa`, FUSE3, and
  `user_allow_other` handling where needed.
- Docs maturity: one page says GitHub sync is available, but the GitHub Sync
  page currently says "Coming soon"; versioning docs say conflict-resolution
  APIs are not available yet, while the API index references resolution fields.
  We should verify live behavior before designing around those APIs.
- Current UX gap: `pillbox push` snapshots arbitrary cwd into rustic. MesaFS
  works best when the workspace itself is the mounted Mesa repository. Supporting
  both arbitrary local cwd snapshots and mounted Mesa repos will need explicit
  product decisions.
- Cost model: current local rustic is free and private. Mesa has a free tier
  and usage-based pricing, but it is still a metered external service.

## Decision

Do not adopt Mesa just to improve the remote workspace story. The codebase
already has the right storage primitive and remote hydration/completion
protocol in rustic S3/R2.

Use Mesa only as a separate prototype if pillbox wants hosted VFS semantics and
first-class diff/merge/review flows. Keep rustic as the default unless a spike
proves:

- MesaFS can run inside our local Docker runner and E2B template.
- A Mesa repo can support the file counts and dependency-install patterns common
  in coding-agent workspaces.
- We can reliably capture current change id at session completion.
- Diffs and merges are good enough for proposal/approval workflows.
- Failure modes are acceptable when Mesa credentials are missing, expired,
  repo-scoped incorrectly, or the service is unavailable.

The end state worth aiming for is not "replace rustic snapshots with Mesa
snapshots"; it is "keep rustic as the durable remote snapshot transport" and,
separately if needed, "make Mesa the branchable review workspace backend for
forked agent sessions."
