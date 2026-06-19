# `pillbox collect` — fan-out result collection for orchestrators

`collect` is the substrate half of the fan-out / merge loop. An
orchestrator (a host `claude`/`codex` session, ghost, or a script) forks
work into independent pillbox sessions, then needs to **retrieve every
finished result and reason about its lineage** — without pillbox deciding
*how* the results get merged.

That boundary is the whole design:

> **pillbox owns the mechanism (collect the trees, report the lineage).
> The orchestrator owns the policy (select one / union / three-way merge /
> hand conflicts to an agent).** `collect` deliberately stops at the merge
> decision.

`collect` is the layer `dispatch` is a special case of: `dispatch` =
`collect` + grade + **select-one** (the simplest merge policy, baked in).

## What it does

Given one or more finished session ids:

1. Rehydrate each session's **result tree** into `<to>/<session>/` —
   `result_snapshot` (canonical, survives `session rm`) first, falling back
   to the live backend workspace clone (libkrun headless sessions that were
   never snapshotted).
2. Report, per session, the **merge triple handles** the orchestrator needs:
   - `base_snapshot` + `base_git_anchor` — the fork point. `base_git_anchor`
     is the git commit the orchestrator uses as the **merge base**.
   - `result_snapshot` + `result_git_anchor` — *theirs*.
   - `dir` — where *theirs* was rehydrated.
3. Emit a `--json` manifest the orchestrator reads to decide.

The orchestrator then does whatever it wants with the trees + handles:
`git checkout <base_git_anchor>`, lay the result dir over it, commit, and
`git merge` — or select one, or octopus the disjoint ones, or feed conflicts
to an agent. pillbox is not in that loop.

`collect` **validates up front**: if any named session has no result yet
(agent hasn't finished, or already torn down), it errors before any rehydrate,
listing the laggards rather than silently collecting a partial set. This is
the no-result guard, not a transactional rollback — a rare mid-batch I/O
failure can leave already-collected dirs on disk, so treat a non-zero exit as
untrusted output (batch-atomic temp+rename is a possible future enhancement).
An orchestrator that wants "collect whatever's ready" filters the session list
itself (it knows what finished via `session wait-idle` / `score`).

## Manifest (`--json`)

Stable; pin against `version: 1`. `dir` / `to` are always absolute (cwd-rooted,
so the manifest is portable to an orchestrator running from a different cwd).
`ref` is `null` unless `--as-refs` synthesized a commit for the result.

```jsonc
{
  "version": 1,
  "pillbox": "myapp",
  "to": "/abs/collected",
  "results": [
    {
      "session": "ab12cd34ef56",
      "base_snapshot":     "<64-hex|null>",   // fork point
      "base_git_anchor":   "<40-hex|null>",   // merge base commit
      "result_snapshot":   "<64-hex|null>",   // theirs (null if from live clone)
      "result_git_anchor": "<40-hex|null>",
      "dir":    "/abs/collected/ab12cd34ef56",
      "source": "snapshot",                    // or "live_clone"
      "ref":    "refs/pillbox/collect/ab12cd34ef56"  // null without --as-refs
    }
  ]
}
```

## CLI

```
pillbox collect SESSION… [--to DIR] [--as-refs] [--json]
```

- `SESSION…` — one or more session ids (unique prefix ≥ 4 chars, same UX as
  `session pull`).
- `--to DIR` — parent directory for rehydrated trees. Default `./collected`.
  Each session lands at `<DIR>/<session>/`. Relative paths are absolutized.
- `--as-refs` — also synthesize a git commit per result under
  `refs/pillbox/collect/<session>` (see below). Requires cwd to be a git work
  tree; adds the `ref` field to each manifest entry.
- `--json` — emit the manifest instead of the human summary.

Exit codes follow the pillbox convention: `0` ok, `1` a named session has no
result / lookup failed / git synthesis failed, `2` usage (incl. `--as-refs`
outside a git work tree).

## `--as-refs` — git-commit synthesis

With `--as-refs`, `collect` projects each rehydrated result tree into the
**originating repo** (cwd) as a git commit and writes it under
`refs/pillbox/collect/<session>`, so an orchestrator merges with plain git
plumbing instead of reimplementing the temp-index dance:

- **tree** = the result tree (built through a throwaway index — the repo's real
  index/working tree are never touched). A nested `.git` in the result (the
  workspace was itself a repo) is moved aside for the scan, so it's neither
  tracked nor mistaken for a submodule.
- **parent** = `base_git_anchor` when that commit exists in the repo, so
  `git diff <base_git_anchor>..<ref>` (or `git merge <ref>`) is exactly the
  worker's change. When the base is absent, the commit is an orphan (you still
  get *theirs*, just no 3-way base).
- **`.gitignore` is respected**, so the ref is a *mergeable code tree*; the
  full workspace (build artifacts, untracked junk) stays available under
  `--to <DIR>/<session>/` for anything that needs it.
- The commit author/committer is a fixed `pillbox` identity, so synthesis never
  depends on the repo's `user.*` config.

pillbox still never merges — it just hands the orchestrator merge-ready refs.

## How an orchestrator uses it (merge stays its job)

```sh
# fan out (dispatch is one policy; a bare run --detach loop is another)
ids=$(... fork k workers, collect their ids ...)

# collect the finished results + lineage, and synthesize merge-ready refs
pillbox collect $ids --to ./collected --as-refs --json > manifest.json

# merge — the orchestrator's policy, the orchestrator's tool, over the refs:
#  select-one:  git merge --ff-only refs/pillbox/collect/<winner>
#  three-way:   git merge refs/pillbox/collect/<id>   (or jj, for first-class
#               conflicts — pillbox takes no jj dependency; jj is the
#               orchestrator's choice in its own workspace)
#  union:       disjoint slices → merge each ref in turn, no conflicts
# (Without --as-refs, the manifest's base_git_anchor + dir still let the
#  orchestrator build the same commits with its own tooling.)
```

## Lineage (the merge-back half)

`collect` reads the **fork** edge straight off the session record
(`base_snapshot` → `result_snapshot`) — it needs nothing new. The
**merge-back** edge — recording that a new snapshot is the merge of a base +
a chosen result — is `pillbox push --parent …` (repeatable, prefix-ok):

```sh
# orchestrator loop: fork → collect → merge → record the merge-back edge
pillbox push --bookmark main --parent <base> --parent <winner>
```

Parents are resolved to full ids against the repo (a typo'd/unknown parent
fails the push) and stored in the snapshot's pillbox metadata — so they're
pillbox-native: they survive `session rm` and work on the S3/R2 backend where
there's no git to read lineage from. `pillbox snapshot show/list [--json]` and
`push --json` surface them as a `parents` array. The result is a legible
workspace DAG: `S0 → {r1..rk}` (forks), `{S0, winner} → S1` (merge), bookmark
`main: S0 → S1`.

## Scope / increments

- **Increment 1 (this):** `--to DIR` + `--json` manifest. Reuses the proven
  `session pull` rehydrate path. The manifest's `base_git_anchor` already
  hands the orchestrator the merge base, so this is a faithful interchange —
  the orchestrator does the git step with its own tooling.
- **Increment 2 (done):** `--as-refs` — git-commit synthesis (above).
- **Increment 3 (done):** `parents` on snapshots + `push --parent` for the
  merge-back lineage edge (above). Merge itself stays out of pillbox — it's the
  orchestrator's policy, with the orchestrator's tool (git/jj).
