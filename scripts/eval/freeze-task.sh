#!/usr/bin/env bash
# Freeze a task directory as an immutable, content-addressed snapshot in the
# `evals` pillbox, bookmarked <set>/<split>/<id> — the eval-context freeze
# ("ShopSnap" for a coding task). Pure composition of `pillbox push --bookmark`:
# the snapshot captures workspace/ + grader/ + prompt.txt TOGETHER, so the
# verifier freezes WITH the starting tree (a changed grader changes the score).
# run-task.sh pulls it back by bookmark for an identical rerun across rounds.
#
# Usage: freeze-task.sh <task-dir> <set> <split> [id]
#   <split> is train|held-out; <id> defaults to basename(task-dir).
#
# Env: PILLBOX (binary), EVALS_PILLBOX (named pillbox, default `evals`).
# One-time setup: create the store pillbox once — `cd <somewhere> && pillbox
# new --name evals` (bookmarks require a project pillbox).
set -euo pipefail

u="usage: freeze-task.sh <task-dir> <set> <split> [id]"
task_dir="${1:?$u}"
set_name="${2:?$u}"
split="${3:?$u}"
id="${4:-$(basename "$task_dir")}"
here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
EVALS_PILLBOX="${EVALS_PILLBOX:-evals}"

[ -d "$task_dir/workspace" ] || { echo "freeze-task: $task_dir has no workspace/" >&2; exit 1; }
[ -d "$task_dir/grader" ]    || { echo "freeze-task: $task_dir has no grader/" >&2; exit 1; }

# push snapshots cwd into the resolved pillbox's repo + binds the bookmark to
# THIS snapshot (no handle-copy / latest race). cwd need not be the pillbox's
# own dir — we use its rustic repo as a content-addressed store.
( cd "$task_dir" && "$PILLBOX" --pillbox "$EVALS_PILLBOX" push \
    --tag "$id" --bookmark "$set_name/$split/$id" --json )
