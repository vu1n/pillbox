#!/usr/bin/env python3
"""distill.py — §0 session trace → durable, model-agnostic, failure-mined memory claims.

The capture→claim half of the swarm-memory loop (store.py is the storage + recall half). Reads a
session's §0 event log (`pillbox session log ID`, i.e. log.jsonl — the Event contract in
src/contract.rs: camelCase envelope, payload internally tagged on `type` in snake_case), compacts it
into a failure-weighted Trace, and writes CLAIMS to the store.

Research-shaped (ReasoningBank / MemCollab, see metaharness-sota-research memory): memory is DISTILLED
guidance learned from FAILURES, not raw trajectories; and SHARED (project/global) claims must be
MODEL-AGNOSTIC — cross-model transfer of model-entangled memory DEGRADES. So distill mines VERIFIABLE
failures (failed rubric criteria, run failure, repeated tool errors) into `pitfall` candidates,
records the producing model as provenance only, and strips the model identity from shared content.

The reasoning step is behind a Distiller seam (mirrors store's BYO embed/resolver). HeuristicDistiller
ships now (deterministic, no LLM); an LLM distiller (LLM_DISTILL_PROMPT is its contract) plugs in for
richer trajectory→claims later — same interface.
"""
from __future__ import annotations

import json
from collections import Counter
from dataclasses import dataclass, field
from typing import Protocol

_MAX = 800  # claim/observation content cap — claims are distilled, not transcripts


@dataclass
class Action:
    name: str
    status: str  # ToolStatus on the wire: completed | error | running | unspecified
    title: str
    input: dict | None
    output: str

    @property
    def failed(self) -> bool:
        return self.status == "error"


@dataclass
class Verdict:
    grader: str
    passed: bool
    score: float
    feedback: str
    criteria: list[dict]  # [{name, passed, feedback}] — the decomposed, verifiable gradient


@dataclass
class Trace:
    session_id: str
    task: str
    models: list[str]  # producing model(s): provenance + the strings stripped from shared claims
    actions: list[Action]
    run_failed: str | None
    verdict: Verdict | None
    event_count: int


@dataclass
class ClaimDraft:
    type: str
    subject: str
    content: str
    confidence: float
    code_refs: list[dict] = field(default_factory=list)


def read_log(path: str) -> list[dict]:
    """Load a §0 log.jsonl (one Event JSON per line) into event dicts."""
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def build_trace(events: list[dict], task: str = "") -> Trace:
    """Compact §0 events into a failure-weighted Trace. Defensive: external JSON, read only what we
    need, tolerate missing fields. `task` is the prompt the session was given (the orchestrator knows
    it; the §0 log doesn't carry it reliably)."""
    sid, models, actions = "", [], []
    run_failed: str | None = None
    verdict: Verdict | None = None
    for ev in events:
        sid = sid or ev.get("sessionId", "")
        p = ev.get("payload") or {}
        t = p.get("type")
        if t == "tool_call":
            actions.append(Action(p.get("name", ""), p.get("status", ""), p.get("title", ""),
                                   p.get("input"), p.get("output", "")))
        elif t == "message_end":
            m = p.get("model", "")
            if m and m not in models:
                models.append(m)
        elif t == "run_failed":
            run_failed = p.get("reason", "") or f"exit {p.get('exitCode')}"
        elif t == "scored":
            verdict = Verdict(p.get("grader", ""), bool(p.get("passed")), float(p.get("score") or 0.0),
                              p.get("feedback", ""), p.get("criteria") or [])
    return Trace(sid, task, models, actions, run_failed, verdict, len(events))


class Distiller(Protocol):
    """Trace → claim drafts. The reasoning step, swappable (heuristic now, LLM later)."""

    def distill(self, trace: Trace) -> list[ClaimDraft]: ...


class HeuristicDistiller:
    """Deterministic FAILURE-MINING — no LLM. Verifiable failures only → `pitfall` candidates:
    failed rubric criteria (the richest, per-criterion verifiable signal), a run failure, and a tool
    that erred repeatedly. Conservative by design — it never invents a 'success procedure' (that's
    where Goodhart/hallucinated memory creeps in; leave success distillation to a judgment-capable
    LLM distiller behind the same seam)."""

    def distill(self, trace: Trace) -> list[ClaimDraft]:
        drafts: list[ClaimDraft] = []
        if trace.verdict:
            failed = [c for c in trace.verdict.criteria if not c.get("passed", True)]
            total = len(trace.verdict.criteria)
            if len(failed) > 3:
                # Broad failure → ONE distilled pitfall. One-per-criterion here is a raw failure
                # list (e.g. 19/20 unit tests), not guidance — it pollutes recall. The count is the
                # signal: many failures = "broadly broken, one lesson"; few = specific edge cases.
                sample = ", ".join(c.get("name", "?") for c in failed[:3])
                drafts.append(ClaimDraft(
                    "pitfall", f"most criteria failed ({len(failed)}/{total})",
                    _trunc(f"{len(failed)} of {total} '{trace.verdict.grader}' criteria failed "
                           f"(e.g. {sample}). {trace.verdict.feedback}"), 0.6))
            else:
                for c in failed:
                    name = c.get("name", "?")
                    drafts.append(ClaimDraft(
                        "pitfall", f"criterion failed: {name}",
                        _trunc(c.get("feedback") or f"the rubric criterion {name!r} failed"), 0.6))
        if trace.run_failed:
            drafts.append(ClaimDraft("pitfall", "run failed", _trunc(trace.run_failed), 0.5))
        errs = Counter(a.name for a in trace.actions if a.failed)
        for name, n in errs.items():
            if n >= 2:
                sample = next(a for a in trace.actions if a.failed and a.name == name)
                drafts.append(ClaimDraft(
                    "pitfall", f"{name} repeatedly failed",
                    _trunc(f"`{name}` errored {n}x this run; e.g. {sample.output}"), 0.4,
                    code_refs=_refs_from(sample.input)))
        return drafts


# Contract for an LLM-backed Distiller (the next slice): same seam, richer judgment. The model reads
# the full Trace and emits ClaimDraft JSON. Non-negotiables baked into the prompt:
#   - mine LESSONS not events: durable, reusable guidance (a future agent acts on it), never chatter;
#   - failure-first (what went wrong + the fix), but success→`procedure` is allowed WITH judgment;
#   - MODEL-AGNOSTIC content for shared scopes (no model names / model-specific phrasing);
#   - GROUND to code via code_refs (symbol/path/recipe) when the lesson is about specific code;
#   - one tight subject + content per claim; set type ∈ store.TYPES and a calibrated confidence.
LLM_DISTILL_PROMPT = (
    "You are distilling a coding-agent session trace into durable memory claims for a swarm of "
    "agents. Emit JSON: a list of {type, subject, content, confidence, code_refs}. Mine reusable "
    "LESSONS (facts, decisions, procedures, pitfalls), not a play-by-play. Prioritize failures and "
    "their fixes. Content must be MODEL-AGNOSTIC (no model names) — it is shared across models and "
    "model-entangled memory degrades transfer. Ground claims to code via code_refs "
    "[{symbol,path,query}] when they concern specific code. Skip one-off chatter and anything with "
    "no clear, reusable subject."
)


def distill_session(events: list[dict], store, *, project: str, scope: str = "project",
                    task: str = "", distiller: Distiller | None = None, actor: str = "distill") -> list[str]:
    """Compact a session's §0 events, mine claims, write them with provenance. The trace is recorded
    as one observation (the claims' source); the producing model goes to provenance metadata, and
    SHARED-scope claim content is stripped of the model identity (the MemCollab cross-model landmine).
    Returns the new claim ids."""
    trace = build_trace(events, task=task)
    distiller = distiller or HeuristicDistiller()
    oid = store.observe(actor=actor, content=_trace_summary(trace), scope=scope, project=project,
                        source=f"session:{trace.session_id}" if trace.session_id else None,
                        metadata={"models": trace.models, "event_count": trace.event_count})
    ids = []
    for d in distiller.distill(trace):
        content = _model_agnostic(d.content, trace.models, scope)
        ids.append(store.claim(d.type, d.subject, content, scope=scope, project=project,
                               confidence=d.confidence, source_ids=[oid], code_refs=d.code_refs))
    return ids


# --- helpers ----------------------------------------------------------------------------------
def _trunc(s: str) -> str:
    s = (s or "").strip()
    return s if len(s) <= _MAX else s[:_MAX] + "…"


def _refs_from(inp: dict | None) -> list[dict]:
    """Best-effort code anchor from a tool input (most file tools carry file_path/path)."""
    if not isinstance(inp, dict):
        return []
    path = inp.get("file_path") or inp.get("path")
    return [{"path": path}] if isinstance(path, str) and path else []


def _model_agnostic(content: str, models: list[str], scope: str) -> str:
    """Strip the producing model's identity from SHARED claims — model-entangled memory degrades
    cross-model transfer (MemCollab). Agent/user-scoped memory keeps it (single-model by definition)."""
    if scope in ("project", "global"):
        for m in models:
            if m:
                content = content.replace(m, "<model>")
    return content


def _trace_summary(t: Trace) -> str:
    parts = []
    if t.task:
        parts.append(f"task: {t.task}")
    failed = sum(a.failed for a in t.actions)
    parts.append(f"{len(t.actions)} tool calls" + (f" ({failed} failed)" if failed else ""))
    if t.run_failed:
        parts.append(f"run failed: {t.run_failed}")
    if t.verdict:
        parts.append(f"verdict: {'pass' if t.verdict.passed else 'fail'} (score {t.verdict.score})")
    return _trunc("; ".join(parts))


if __name__ == "__main__":
    import glob
    import os
    import sys

    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from store import MemoryStore

    db = "/tmp/distill-selftest.db"
    for f in glob.glob(db + "*"):
        try: os.remove(f)
        except OSError: pass

    model = "claude-opus-4-8"
    # synthetic §0 log: a model turn, a tool that errors twice (file anchor), a failed rubric
    # criterion (whose feedback leaks the model name → must be stripped), and a run failure.
    events = [
        {"sessionId": "sess1", "payload": {"type": "message_start", "messageId": "m1", "role": "assistant"}},
        {"sessionId": "sess1", "payload": {"type": "message_end", "messageId": "m1", "model": model}},
        {"sessionId": "sess1", "payload": {"type": "tool_call", "toolCallId": "t1", "name": "Bash",
                                           "status": "error", "output": "cargo: no such feature",
                                           "input": {"command": "cargo build"}}},
        {"sessionId": "sess1", "payload": {"type": "tool_call", "toolCallId": "t2", "name": "Bash",
                                           "status": "error", "output": "still failing",
                                           "input": {"command": "cargo build"}}},
        {"sessionId": "sess1", "payload": {"type": "tool_call", "toolCallId": "t3", "name": "Edit",
                                           "status": "error", "output": "no match",
                                           "input": {"file_path": "src/sandbox/mod.rs"}}},
        {"sessionId": "sess1", "payload": {"type": "tool_call", "toolCallId": "t4", "name": "Edit",
                                           "status": "error", "output": "no match",
                                           "input": {"file_path": "src/sandbox/mod.rs"}}},
        {"sessionId": "sess1", "payload": {"type": "scored", "grader": "rubric", "passed": False,
                                           "score": 0.5, "feedback": "1/2",
                                           "criteria": [{"name": "builds", "passed": False,
                                                         "feedback": f"build failed under {model}; missing --features libkrun"},
                                                        {"name": "tests", "passed": True, "feedback": ""}]}},
        {"sessionId": "sess1", "payload": {"type": "run_failed", "reason": "agent exited 1", "exitCode": 1}},
    ]

    store = MemoryStore(db)
    ids = distill_session(events, store, project="pillbox", task="add libkrun feature flag")
    claims = store.recall("build libkrun feature failed", project="pillbox", include_candidates=True)
    kinds = {c.subject for c in claims}

    assert ids, "distilled no claims"
    assert any("criterion failed: builds" == c.subject for c in claims), kinds
    assert any(c.subject == "Bash repeatedly failed" for c in claims), kinds
    assert any(c.subject == "run failed" for c in claims), kinds
    # all pitfalls are candidates (arbiter accepts later), sourced, model-agnostic
    assert all(c.status == "candidate" for c in claims), [(c.subject, c.status) for c in claims]
    assert all(c.source_ids for c in claims), "claims must carry provenance"
    builds = next(c for c in claims if c.subject == "criterion failed: builds")
    assert model not in builds.content and "<model>" in builds.content, builds.content
    # the repeated-Edit pitfall is code-anchored from the erroring tool's file_path input
    edit_pf = next(c for c in claims if c.subject == "Edit repeatedly failed")
    assert edit_pf.code_refs and edit_pf.code_refs[0]["path"] == "src/sandbox/mod.rs", edit_pf.code_refs

    print(f"OK — distilled {len(ids)} pitfall candidate(s) from a {len(events)}-event §0 trace: "
          f"{sorted(kinds)}; failure-mined + model-agnostic + sourced")
