# Bounded repository execution smoke

This tool prepares a three-file Git fixture and one sealed execution/3 request. Preparation and
inspection are offline: they do not launch Pillbox, a VM, authentication, Docker, or a model.
Python 3 and Git are required. All artifact paths are supplied by the caller; paths with spaces work.
Generated files are immutable on rerun, and a dirty or changed fixture is rejected.

```sh
scripts/smoke/repository-execution.sh \
  --artifacts /private/tmp/pillbox-repository-proof \
  --image sha256:3af5c7cbb9e111e817960a30b01983f9b3891a7efb811a124c2a35d22c7126b2
```

The request fixes Codex 0.151.0, `gpt-5.6-sol`, profile `sol`, effort `low`, ten tool calls, and a
600-second invocation deadline. It grants reads of `docs/task.txt` and `src/answer.txt`, and a write
only to `src/answer.txt`. The requested edit is `answer=0\n` to `answer=42\n`. A third file contains a
synthetic scope canary. The sealed offline verifier checks the complete result file set, bytes,
and executable bits. No removal, shell, or wider path capability is requested.

Preparation writes `repository/`, `pillbox.toml`, and `generated/` containing the request, changed
retry, canonical manifests, and identity hashes. The project descriptor isolates execution state
while the runtime uses the existing host-managed Codex login. The smoke code never reads login files.

## Explicit live selection

Run this only after the independent VM hardening and process-ownership gates pass and live execution
has been authorized. This command can consume one model turn. The caller selects the exact signed
libkrun binary, immutable image ID, and artifact directory; there is no fallback model or retry loop.

```sh
scripts/smoke/repository-execution.sh live \
  --binary /absolute/path/to/signed/pillbox \
  --artifacts /private/tmp/pillbox-repository-proof \
  --image sha256:3af5c7cbb9e111e817960a30b01983f9b3891a7efb811a124c2a35d22c7126b2
```

Live mode currently requires macOS. It verifies the code signature and Hypervisor entitlement,
records the exact binary hash/version, obtains the project state directory through `info --json`,
and cross-checks the runtime's immutable snapshot digest before calling `execution execute` once.
It then reads status and inspects actual evidence. The 660-second outer timeout preserves runtime
stdout/stderr and stops the foreground owner if it expires; the runtime's ownership watchdogs must
provide descendant teardown. An outer timeout is failure, not teardown proof.

Every command's arguments, status, stdout, and stderr stay in `observations/live/`, including failed
attempts. That directory's existence prevents another live call from this tool. Keep failures and
investigate before deliberately selecting any new invocation; do not delete the directory to retry.
A completed CLI state alone is insufficient: `checked.json` appears only after all evidence checks.

## Inspect existing evidence offline

```sh
scripts/smoke/repository-execution.sh inspect \
  --artifacts /private/tmp/pillbox-repository-proof \
  --completion /private/tmp/pillbox-repository-proof/observations/live/execute.json \
  --state-dir /absolute/path/from/the/recorded/info.json
```

The inspector verifies request/result/verifier bindings, numeric SessionLog positions, separate
builder/verifier sessions, referenced artifact lengths and SHA-256 hashes, every complete snapshot
file, the verifier's actual exit and failure flags, and bounded output. It applies the captured patch
to a fresh copy of the committed base and requires the complete resulting manifest to match,
including executable bits. The source repository remains unchanged.

Native evidence must contain exactly one `turn/start`, the requested model/effort in both the
outbound turn and observed thread response, one correlated completed turn, no model rerouting,
only granted file calls, both required reads, and the exact UTF-8 or base64 write. The synthetic
ungranted canary must be absent from native evidence. This checks runtime observations, not provider
internals, credential confinement, or OS isolation; those retain their separate implementation and
actual VM gates. Test-generated receipts are never evidence of a live run.

## Explicit retry proof

After a successful checked live run, this mode repeats the identical request, then submits the
same invocation with changed content. It uses the original binary and project, requires identical
terminal records and unchanged invocation/session file fingerprints, and requires an explicit
conflict containing both request hashes. It does not create another invocation ID.

```sh
scripts/smoke/repository-execution.sh verify-retry \
  --binary /absolute/path/to/the/same/signed/pillbox \
  --artifacts /private/tmp/pillbox-repository-proof \
  --image sha256:3af5c7cbb9e111e817960a30b01983f9b3891a7efb811a124c2a35d22c7126b2
```

Outputs are retained in the once-only `observations/retry/` directory. Fingerprints include only
`repository-executions/` and `sessions/`, never authentication paths. The standalone `fingerprint`
mode takes `--state-dir` for read-only inspection. Cancellation and supervisor-crash tests remain
manual, separately selected invocations; they require actual owner/group teardown and recovered
terminal-state evidence. This helper does not automate them or infer success from intent.

## Offline regression checks

```sh
bash -n scripts/smoke/repository-execution.sh
python3 -m unittest discover -s scripts/smoke/repository-execution -p 'test_*.py' -v
```

These use temporary Git repositories and explicitly synthetic local evidence. They cover deterministic
portable preparation, immutable-artifact/dirty-input rejection, both content codecs, patch reproduction,
tampered blobs, duplicate turns, changed models, outside-grant calls, verifier session separation,
auth-free fingerprint scope, preserved command failure, and prevention of live relaunch.
