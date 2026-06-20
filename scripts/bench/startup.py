#!/usr/bin/env python3
"""Benchmark pillbox sandbox startup timings.

The script consumes the lifecycle fields emitted by host-side
`session.started` events:

  - startup_ms
  - startup_stages: [{name, duration_ms}, ...]

It runs configured cases, captures each run's session id from `pillbox run
--json`, reads `pillbox session events --json`, and summarizes totals plus
per-stage p50/p95. The harness deliberately stays outside pillbox proper so the
measurement surface can be used before and after each optimization PR.
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import os
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional, Union


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_PILLBOX = ROOT / "target" / "debug" / "pillbox"


@dataclass(frozen=True)
class Case:
    name: str
    backend: str
    agent: str
    description: str
    needs_model: bool = False
    runner_image: Optional[str] = None


CASES: dict[str, Case] = {
    "docker-claude": Case(
        "docker-claude",
        "docker",
        "claude",
        "Docker backend, Claude PTY session",
    ),
    "docker-codex": Case(
        "docker-codex",
        "docker",
        "codex",
        "Docker backend, Codex PTY session",
    ),
    "docker-opencode": Case(
        "docker-opencode",
        "docker",
        "opencode",
        "Docker backend, opencode server session",
        needs_model=True,
        runner_image="pillbox-runner:l7",
    ),
    "libkrun-claude": Case(
        "libkrun-claude",
        "libkrun",
        "claude",
        "libkrun backend, Claude PTY session",
    ),
    "libkrun-codex": Case(
        "libkrun-codex",
        "libkrun",
        "codex",
        "libkrun backend, Codex PTY session",
    ),
    "libkrun-opencode": Case(
        "libkrun-opencode",
        "libkrun",
        "opencode",
        "libkrun backend, opencode server session",
        needs_model=True,
        runner_image="pillbox-runner:l7",
    ),
    "libkrun-codex-serve": Case(
        "libkrun-codex-serve",
        "libkrun",
        "codex-serve",
        "libkrun backend, codex app-server session",
        needs_model=True,
        runner_image="pillbox-runner:l8",
    ),
}


def main() -> int:
    args = parse_args()
    if args.list_cases:
        list_cases()
        return 0

    case_names = selected_cases(args)
    if not case_names:
        print(
            "startup-bench: select at least one --case, --all-docker, --all-libkrun, or --all",
            file=sys.stderr,
        )
        return 2

    pillbox = resolve_pillbox(args.pillbox)
    workspace_guard = None
    workspace = Path(args.workspace).resolve() if args.workspace else None
    if workspace is None:
        workspace_guard = tempfile.TemporaryDirectory(prefix="pillbox-startup-bench-")
        workspace = Path(workspace_guard.name)
        (workspace / "README.md").write_text("startup benchmark workspace\n", encoding="utf-8")

    base_cmd = [str(pillbox)]
    if args.pillbox_name:
        base_cmd.extend(["--pillbox", args.pillbox_name])

    runs: list[dict[str, Any]] = []
    failures: list[dict[str, str]] = []
    try:
        for name in case_names:
            case = CASES[name]
            total = args.warmup + args.repeat
            for index in range(total):
                measured = index >= args.warmup
                label = f"startup-bench-{case.name}-{uuid.uuid4().hex[:8]}"
                run = run_case(
                    args=args,
                    case=case,
                    base_cmd=base_cmd,
                    workspace=workspace,
                    label=label,
                    measured=measured,
                )
                if run.get("ok"):
                    if measured:
                        runs.append(run)
                else:
                    failures.append(
                        {
                            "case": case.name,
                            "label": label,
                            "reason": str(run.get("reason", "unknown failure")),
                        }
                    )
    finally:
        if workspace_guard is not None:
            workspace_guard.cleanup()

    if args.dry_run:
        return 0
    if args.output_json:
        print(
            json.dumps(
                {"runs": runs, "summary": summarize(runs), "failures": failures},
                indent=2,
                sort_keys=True,
            )
        )
    elif args.output_csv:
        print_csv(runs)
    else:
        print_human(runs, failures)
    return 1 if failures else 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run pillbox startup cases and summarize host session.started startup timings.",
    )
    parser.add_argument(
        "--case",
        action="append",
        choices=sorted(CASES),
        default=[],
        help="Case to run. Repeatable.",
    )
    parser.add_argument("--all-docker", action="store_true", help="Run every Docker case.")
    parser.add_argument("--all-libkrun", action="store_true", help="Run every libkrun case.")
    parser.add_argument("--all", action="store_true", help="Run every known case.")
    parser.add_argument("--list-cases", action="store_true", help="List cases and exit.")
    parser.add_argument(
        "--repeat",
        type=positive_int,
        default=5,
        help="Measured runs per case. Default: 5.",
    )
    parser.add_argument(
        "--warmup",
        type=nonnegative_int,
        default=1,
        help="Unmeasured warmup runs per case. Default: 1.",
    )
    parser.add_argument("--workspace", help="Workspace to mount. Default: temporary empty workspace.")
    parser.add_argument(
        "--workspace-name",
        default="startup-bench",
        help="Guest /workspace mount name. Default: startup-bench.",
    )
    parser.add_argument("--ttl", default="30m", help="Detached session TTL. Default: 30m.")
    parser.add_argument("--model", default=os.environ.get("MODEL"), help="Model for server-mode agents.")
    parser.add_argument(
        "--runner-image",
        default=os.environ.get("PILLBOX_RUNNER_IMAGE"),
        help="Override PILLBOX_RUNNER_IMAGE for every case.",
    )
    parser.add_argument(
        "--pillbox",
        default=os.environ.get("PILLBOX"),
        help="Pillbox binary. Default: target/debug/pillbox, then PATH.",
    )
    parser.add_argument("--pillbox-name", help="Pass --pillbox NAME to the CLI.")
    parser.add_argument(
        "--cwd",
        default=os.getcwd(),
        help="Directory where pillbox commands run. Default: current directory.",
    )
    parser.add_argument(
        "--timeout",
        type=positive_int,
        default=300,
        help="Per pillbox command timeout in seconds. Default: 300.",
    )
    parser.add_argument("--dry-run", action="store_true", help="Print commands without running them.")
    parser.add_argument("--json", dest="output_json", action="store_true", help="Print JSON results.")
    parser.add_argument("--csv", dest="output_csv", action="store_true", help="Print CSV rows for measured runs.")
    return parser.parse_args()


def positive_int(raw: str) -> int:
    value = int(raw)
    if value < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return value


def nonnegative_int(raw: str) -> int:
    value = int(raw)
    if value < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return value


def resolve_pillbox(raw: Optional[str]) -> Union[Path, str]:
    if raw:
        return Path(raw).resolve() if "/" in raw else raw
    if DEFAULT_PILLBOX.exists():
        return DEFAULT_PILLBOX
    found = shutil.which("pillbox")
    return found if found else "pillbox"


def selected_cases(args: argparse.Namespace) -> list[str]:
    names: list[str] = []
    if args.all:
        names.extend(CASES)
    if args.all_docker:
        names.extend(name for name, case in CASES.items() if case.backend == "docker")
    if args.all_libkrun:
        names.extend(name for name, case in CASES.items() if case.backend == "libkrun")
    names.extend(args.case)
    out: list[str] = []
    seen: set[str] = set()
    for name in names:
        if name not in seen:
            seen.add(name)
            out.append(name)
    return out


def list_cases() -> None:
    width = max(len(name) for name in CASES)
    for name, case in CASES.items():
        image = f" image={case.runner_image}" if case.runner_image else ""
        print(f"{name:<{width}}  backend={case.backend:<7} agent={case.agent:<11}{image} {case.description}")


def run_case(
    *,
    args: argparse.Namespace,
    case: Case,
    base_cmd: list[str],
    workspace: Path,
    label: str,
    measured: bool,
) -> dict[str, Any]:
    cmd = build_run_cmd(args, case, base_cmd, workspace, label)
    env = os.environ.copy()
    if case.backend == "libkrun":
        env["PILLBOX_BACKEND"] = "libkrun"
    else:
        env.pop("PILLBOX_BACKEND", None)
    runner_image = args.runner_image or case.runner_image
    if runner_image:
        env["PILLBOX_RUNNER_IMAGE"] = runner_image

    prefix = "measure" if measured else "warmup"
    if args.dry_run:
        print(f"[{prefix}] {case.name}: {env_prefix(case, runner_image)}{shell_join(cmd)}")
        return {"ok": True, "case": case.name, "dry_run": True}

    started = time.time()
    try:
        proc = subprocess.run(
            cmd,
            cwd=args.cwd,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=args.timeout,
        )
    except subprocess.TimeoutExpired as exc:
        elapsed_ms = int((time.time() - started) * 1000)
        output = tail_timeout_output(exc)
        return {
            "ok": False,
            "case": case.name,
            "reason": f"run timed out after {args.timeout}s ({elapsed_ms}ms elapsed): {output}",
        }
    elapsed_ms = int((time.time() - started) * 1000)
    if proc.returncode != 0:
        return {
            "ok": False,
            "case": case.name,
            "reason": tail(proc.stderr or proc.stdout),
        }

    session_id = parse_session_id(proc.stdout)
    if not session_id:
        return {
            "ok": False,
            "case": case.name,
            "reason": f"no session id in stdout: {tail(proc.stdout)}",
        }

    try:
        event = find_started_event(base_cmd, args.cwd, env, session_id, args.timeout)
        if event is None:
            return {"ok": False, "case": case.name, "reason": f"no host session.started event for {session_id}"}
        stages = event.get("startup_stages") or []
        return {
            "ok": True,
            "case": case.name,
            "backend": event.get("backend") or case.backend,
            "agent_id": event.get("agent_id") or case.agent,
            "session_id": session_id,
            "startup_ms": event.get("startup_ms"),
            "startup_stages": stages,
            "run_elapsed_ms": elapsed_ms,
            "measured": measured,
        }
    finally:
        cleanup_session(base_cmd, args.cwd, env, session_id, args.timeout)


def build_run_cmd(
    args: argparse.Namespace,
    case: Case,
    base_cmd: list[str],
    workspace: Path,
    label: str,
) -> list[str]:
    cmd = [
        *base_cmd,
        "run",
        "--agent",
        case.agent,
        "--workspace",
        str(workspace),
        "--name",
        args.workspace_name,
        "--detach",
        "--label",
        label,
        "--ttl",
        args.ttl,
        "--json",
    ]
    if case.needs_model and args.model:
        cmd.extend(["--model", args.model])
    return cmd


def env_prefix(case: Case, runner_image: Optional[str]) -> str:
    parts = []
    if case.backend == "libkrun":
        parts.append("PILLBOX_BACKEND=libkrun")
    if runner_image:
        parts.append(f"PILLBOX_RUNNER_IMAGE={shell_quote(runner_image)}")
    return "".join(f"{part} " for part in parts)


def parse_session_id(stdout: str) -> Optional[str]:
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    session = payload.get("session")
    if isinstance(session, dict):
        sid = session.get("id")
        return sid if isinstance(sid, str) and sid else None
    return None


def find_started_event(
    base_cmd: list[str],
    cwd: str,
    env: dict[str, str],
    session_id: str,
    timeout: int,
) -> Optional[dict[str, Any]]:
    try:
        proc = subprocess.run(
            [*base_cmd, "session", "events", "--json"],
            cwd=cwd,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return None
    if proc.returncode != 0:
        return None
    found: Optional[dict[str, Any]] = None
    for line in proc.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            event.get("session_id") == session_id
            and event.get("event") == "session.started"
            and event.get("emitter") == "host"
        ):
            found = event
    return found


def cleanup_session(
    base_cmd: list[str],
    cwd: str,
    env: dict[str, str],
    session_id: str,
    timeout: int,
) -> None:
    try:
        subprocess.run(
            [*base_cmd, "session", "rm", session_id],
            cwd=cwd,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=min(timeout, 30),
        )
    except subprocess.TimeoutExpired:
        pass


def summarize(runs: list[dict[str, Any]]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for case in sorted({run["case"] for run in runs}):
        case_runs = [run for run in runs if run["case"] == case]
        totals = [int(run["startup_ms"]) for run in case_runs if isinstance(run.get("startup_ms"), int)]
        stages: dict[str, list[int]] = {}
        for run in case_runs:
            for stage in run.get("startup_stages") or []:
                name = stage.get("name")
                dur = stage.get("duration_ms")
                if isinstance(name, str) and isinstance(dur, int):
                    stages.setdefault(name, []).append(dur)
        out[case] = {
            "n": len(case_runs),
            "startup_ms": stats(totals),
            "stages": {name: stats(values) for name, values in sorted(stages.items())},
        }
    return out


def stats(values: list[int]) -> dict[str, Optional[int]]:
    if not values:
        return {"min": None, "p50": None, "p95": None, "max": None}
    ordered = sorted(values)
    return {
        "min": ordered[0],
        "p50": percentile(ordered, 50),
        "p95": percentile(ordered, 95),
        "max": ordered[-1],
    }


def percentile(ordered: list[int], pct: int) -> int:
    if not ordered:
        raise ValueError("percentile needs at least one value")
    index = max(
        0,
        min(len(ordered) - 1, math.ceil((pct / 100.0) * len(ordered)) - 1),
    )
    return ordered[index]


def print_human(runs: list[dict[str, Any]], failures: list[dict[str, str]]) -> None:
    summary = summarize(runs)
    if not summary:
        print("(no measured startup events captured)")
    for case, data in summary.items():
        total = data["startup_ms"]
        print(
            f"{case}: n={data['n']} startup_ms "
            f"min={total['min']} p50={total['p50']} p95={total['p95']} max={total['max']}"
        )
        for stage, st in data["stages"].items():
            print(f"  {stage:<24} min={st['min']} p50={st['p50']} p95={st['p95']} max={st['max']}")
    if failures:
        print("\nfailures:", file=sys.stderr)
        for fail in failures:
            print(f"  {fail['case']} [{fail['label']}]: {fail['reason']}", file=sys.stderr)


def print_csv(runs: list[dict[str, Any]]) -> None:
    writer = csv.writer(sys.stdout)
    writer.writerow(
        ["case", "session_id", "backend", "agent_id", "startup_ms", "stage_name", "stage_duration_ms"]
    )
    for run in runs:
        stages = run.get("startup_stages") or []
        if not stages:
            writer.writerow(
                [
                    run["case"],
                    run["session_id"],
                    run["backend"],
                    run["agent_id"],
                    run.get("startup_ms"),
                    "",
                    "",
                ]
            )
            continue
        for stage in stages:
            writer.writerow(
                [
                    run["case"],
                    run["session_id"],
                    run["backend"],
                    run["agent_id"],
                    run.get("startup_ms"),
                    stage.get("name"),
                    stage.get("duration_ms"),
                ]
            )


def tail(text: str, limit: int = 1200) -> str:
    text = text.strip()
    return text[-limit:] if len(text) > limit else text


def tail_timeout_output(exc: subprocess.TimeoutExpired, limit: int = 1200) -> str:
    parts = []
    for value in (exc.stderr, exc.stdout):
        if isinstance(value, bytes):
            value = value.decode("utf-8", "replace")
        if isinstance(value, str) and value.strip():
            parts.append(value)
    return tail("\n".join(parts), limit) if parts else "no output"


def shell_join(cmd: list[str]) -> str:
    return " ".join(shell_quote(part) for part in cmd)


def shell_quote(part: str) -> str:
    if not part:
        return "''"
    safe = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_+-=.,:/")
    if all(ch in safe for ch in part):
        return part
    return "'" + part.replace("'", "'\\''") + "'"


if __name__ == "__main__":
    raise SystemExit(main())
