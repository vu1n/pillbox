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
import os
import re
import sys
from collections import Counter
from dataclasses import dataclass, field
from typing import Protocol

from .vocab import TYPES  # single-sourced vocabulary (turso-free leaf), shared with store.py

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

    @property
    def failed_criteria(self) -> list[dict]:
        """The one definition of 'a criterion failed' (absent `passed` ⇒ passed). Read by both
        distillers and wire's observe_events — never re-spell the `.get("passed", True)` predicate."""
        return [c for c in self.criteria if not c.get("passed", True)]


@dataclass
class Trace:
    session_id: str
    task: str
    models: list[str]  # producing model(s): provenance + the strings stripped from shared claims
    actions: list[Action]
    run_failed: str | None
    verdict: Verdict | None
    event_count: int

    @property
    def tool_failures(self) -> Counter:
        """Named tool errors tallied by tool — the one definition, read by the heuristic distiller and
        wire's observe_events. (Unnamed failed actions are excluded — nothing to attribute.)"""
        return Counter(a.name for a in self.actions if a.failed and a.name)


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
            failed = trace.verdict.failed_criteria
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
        for name, n in trace.tool_failures.items():
            if n >= 2:
                sample = next(a for a in trace.actions if a.failed and a.name == name)
                drafts.append(ClaimDraft(
                    "pitfall", f"{name} repeatedly failed",
                    _trunc(f"`{name}` errored {n}x this run; e.g. {sample.output}"), 0.4,
                    code_refs=_refs_from(sample.input)))
        return drafts


# LLM_DISTILL_PROMPT + _SCHEMA are what the model is ASKED for (model-agnostic, code-grounded claims).
# The guarantees don't trust it to comply: distill_session strips model identity and parse() validates
# type/shape downstream. Prompt = best-effort; the store path = enforcement.
LLM_DISTILL_PROMPT = (
    "You are distilling a coding-agent session trace into durable memory claims for a swarm of "
    "agents. Emit JSON: a list of {type, subject, content, confidence, code_refs}. Mine reusable "
    "LESSONS (facts, decisions, procedures, pitfalls), not a play-by-play. Prioritize failures and "
    "their fixes. Content must be MODEL-AGNOSTIC (no model names) — it is shared across models and "
    "model-entangled memory degrades transfer. Ground claims to code via code_refs "
    "[{symbol,path,query}] when they concern specific code. Skip one-off chatter and anything with "
    "no clear, reusable subject."
)

_SCHEMA = (
    'Output ONLY a JSON array, no prose. Each element: {"type": one of '
    "fact|preference|decision|procedure|artifact|hypothesis|pitfall, "
    '"subject": a short noun phrase, "content": the durable lesson, "confidence": 0.0-1.0, '
    '"code_refs": [{"symbol":"…","path":"…","query":"…"}] (omit or [] when not about specific code)}. '
    "Return [] if nothing durable."
)


class LLMDistiller:
    """Distiller backed by a BYO `complete(prompt)->str`. Renders the full Trace as RICH text (the
    trajectory, failures, and per-criterion verdict — research says feed traces, not a scalar), asks
    the model for claims, and parses the JSON. Unlike HeuristicDistiller it can generalize — success
    procedures, cross-failure lessons — but its OUTPUT still flows through the same store governance
    (candidates, model-agnostic strip, provenance). The backend is the caller's: local ollama
    (`ollama_complete`), `claude -p`, or an API — distill only needs str→str."""

    def __init__(self, complete, *, max_claims: int = 8):
        self.complete = complete
        self.max_claims = max_claims

    def distill(self, trace: Trace) -> list[ClaimDraft]:
        return self.parse(self.complete(self.render_prompt(trace)))[: self.max_claims]

    def render_prompt(self, trace: Trace) -> str:
        lines = [LLM_DISTILL_PROMPT, "", _SCHEMA, "", "## Session"]
        if trace.task:
            lines.append(f"task: {trace.task}")
        v = trace.verdict
        if v:
            lines.append(f"verdict: {'pass' if v.passed else 'fail'} score={v.score} grader={v.grader}")
            for c in v.failed_criteria:
                lines.append(f"  FAILED {c.get('name', '?')}: {_clip(c.get('feedback') or '', 200)}")
        if trace.run_failed:
            lines.append(f"run failed: {trace.run_failed}")
        lines.append("## Actions")
        for i, a in enumerate(trace.actions[:60], 1):
            path = ""
            if isinstance(a.input, dict):
                p = a.input.get("file_path") or a.input.get("path")
                path = f"({p})" if isinstance(p, str) else ""
            lines.append(f"{i}. [{'error' if a.failed else 'ok'}] {a.name}{path}: {_clip(a.output or a.title, 200)}")
        if len(trace.actions) > 60:
            lines.append(f"… {len(trace.actions) - 60} more actions")
        return "\n".join(lines)

    @staticmethod
    def parse(raw: str) -> list[ClaimDraft]:
        """Extract the JSON from the completion, validate each item, drop the unusable. Raises (loud,
        not silent) when no parseable JSON is present. Prefers a fenced ```json block — models usually
        emit one, and it's robust against prose that itself contains brackets (e.g. "see [below]:")."""
        text = (raw or "").strip()
        fence = re.search(r"```(?:json)?\s*(.+?)```", text, re.DOTALL)
        if fence:
            blob = fence.group(1).strip()
        else:
            obj, arr = text.find("{"), text.find("[")
            start = arr if arr != -1 and (obj == -1 or arr < obj) else obj
            end = text.rfind("]") if start == arr else text.rfind("}")
            if start == -1 or end == -1 or end < start:
                raise ValueError(f"distiller returned no JSON: {text[:200]!r}")
            blob = text[start:end + 1]
        try:
            data = json.loads(blob)
        except json.JSONDecodeError as e:
            raise ValueError(f"distiller returned unparseable JSON: {e}; {blob[:200]!r}") from e
        items = data.get("claims", []) if isinstance(data, dict) else data
        if not isinstance(items, list):
            raise ValueError(f"distiller JSON is not a claim list: {type(items).__name__}")
        drafts: list[ClaimDraft] = []
        for it in items:
            if not isinstance(it, dict):
                continue
            subject, content = (it.get("subject") or "").strip(), (it.get("content") or "").strip()
            if not subject or not content:
                continue  # no clear subject/content → the arbiter would reject it anyway
            typ = it.get("type") if it.get("type") in TYPES else "fact"
            try:
                conf = max(0.0, min(1.0, float(it.get("confidence", 0.7))))
            except (TypeError, ValueError):
                conf = 0.7
            refs = it.get("code_refs")
            refs = [r for r in refs if isinstance(r, dict)] if isinstance(refs, list) else []
            drafts.append(ClaimDraft(typ, subject, _trunc(content), conf, refs))
        return drafts


def ollama_complete(model: str, host: str = "http://127.0.0.1:11434", *,
                    temperature: float = 0.0, timeout: float = 300):
    """A BYO `complete` over a local ollama server (the libkrun local-model forward target). Use:
    `LLMDistiller(ollama_complete('qwen3'))`. temperature 0 for reproducible distillation. timeout is
    generous — a large local model (e.g. 35B) can take >3min/call; on a genuine stall FallbackDistiller
    catches the timeout and uses the heuristic floor."""
    import urllib.request

    def complete(prompt: str) -> str:
        body = json.dumps({"model": model, "prompt": prompt, "stream": False,
                           "options": {"temperature": temperature}}).encode()
        req = urllib.request.Request(host + "/api/generate", data=body,
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read())["response"]

    return complete


class FallbackDistiller:
    """Compose distillers: try `primary` (e.g. the LLM), fall back to `fallback` (e.g. heuristic) when
    it raises — so a flaky or absent model never drops a session to zero claims (the heuristic is the
    verifiable floor). The fallback is RECORDED to stderr, not silently swallowed."""

    def __init__(self, primary: Distiller, fallback: Distiller):
        self.primary, self.fallback = primary, fallback

    def distill(self, trace: Trace) -> list[ClaimDraft]:
        try:
            return self.primary.distill(trace)
        except Exception as e:  # LLM backends fail many ways (connect/timeout/parse) — degrade, don't drop
            print(f"distill: {type(self.primary).__name__} failed ({type(e).__name__}: {str(e)[:120]}); "
                  f"using {type(self.fallback).__name__}", file=sys.stderr)
            return self.fallback.distill(trace)


def distiller_from_env() -> Distiller:
    """The configured distiller for the capture loop. GHOST_DISTILL_MODEL set → the LLM (ollama) with
    a heuristic fallback; unset → heuristic only. GHOST_OLLAMA_HOST overrides the server. The seam-
    config analog of store.store_from_env."""
    model = os.environ.get("GHOST_DISTILL_MODEL")
    if not model:
        return HeuristicDistiller()
    host = os.environ.get("GHOST_OLLAMA_HOST", "http://127.0.0.1:11434")
    timeout = float(os.environ.get("GHOST_OLLAMA_TIMEOUT", "300"))
    return FallbackDistiller(LLMDistiller(ollama_complete(model, host, timeout=timeout)), HeuristicDistiller())


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


def _clip(s: str, n: int) -> str:
    s = " ".join((s or "").split())  # collapse whitespace for compact prompt lines
    return s if len(s) <= n else s[:n] + "…"


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
                # word-boundary replace: strip the model id as a token, not as a substring inside an
                # unrelated word (a short/family id like "o1"/"pi" inside "4o1ms"/"pipeline").
                content = re.sub(rf"\b{re.escape(m)}\b", "<model>", content)
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

    from .store import MemoryStore

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

    # --- LLM distiller (stub backend — deterministic, no live model) ---
    canned = """Sure, here are the claims:
```json
[
  {"type":"decision","subject":"book discount pricing","content":"price groups by distinct-title count; balance group sizes rather than maximizing each group","confidence":0.8},
  {"type":"pitfall","subject":"greedy grouping overcharges","content":"a greedy max-group strategy on glm-4.6 overcharges vs balanced groups","confidence":0.7,"code_refs":[{"symbol":"price","path":"src/bookstore.py"}]},
  {"type":"fact","subject":"","content":"no subject -> must be skipped"},
  {"type":"bogus","subject":"weird type","content":"unknown type coerces to fact"}
]
```
done."""

    def stub_complete(prompt):
        assert "## Actions" in prompt and "task:" in prompt, "prompt missing rendered trace"
        return canned

    # parse robustness: prose containing brackets before a fenced array (the find/rfind heuristic
    # would mis-slice; the fence path handles it), and a non-list payload raises loud.
    prose = "See the items [below] for details:\n```json\n[{\"type\":\"fact\",\"subject\":\"s\",\"content\":\"c\"}]\n```"
    assert [d.subject for d in LLMDistiller.parse(prose)] == ["s"], "fenced array after prose brackets"
    for bad in ('```json\n42\n```', "not json at all"):
        try:
            LLMDistiller.parse(bad)
            raise AssertionError(f"expected ValueError for {bad!r}")
        except ValueError:
            pass

    dl = LLMDistiller(stub_complete)
    drafts = dl.distill(build_trace(events, task="price books"))

    # FallbackDistiller: primary raises → heuristic floor runs (never zero claims); primary OK → wins.
    class _Boom:
        def distill(self, _trace): raise RuntimeError("model down")
    assert FallbackDistiller(_Boom(), HeuristicDistiller()).distill(build_trace(events)), "fallback floor"
    assert FallbackDistiller(dl, HeuristicDistiller()).distill(build_trace(events, task="t"))[0].subject \
        == "book discount pricing", "primary wins when it succeeds"
    # distiller_from_env: model unset → heuristic; set → LLM-with-fallback
    os.environ.pop("GHOST_DISTILL_MODEL", None)
    assert isinstance(distiller_from_env(), HeuristicDistiller)
    os.environ["GHOST_DISTILL_MODEL"] = "fake-model"
    assert isinstance(distiller_from_env(), FallbackDistiller)
    os.environ.pop("GHOST_DISTILL_MODEL", None)
    assert [d.subject for d in drafts] == ["book discount pricing", "greedy grouping overcharges",
                                           "weird type"], [d.subject for d in drafts]  # blank-subject dropped
    assert drafts[2].type == "fact"  # unknown type coerced

    db2 = "/tmp/distill-llm-selftest.db"
    for f in glob.glob(db2 + "*"):
        try: os.remove(f)
        except OSError: pass
    s2 = MemoryStore(db2)
    ev2 = events + [{"sessionId": "sess2", "payload": {"type": "message_end", "messageId": "x", "model": "glm-4.6"}}]
    ids2 = distill_session(ev2, s2, project="pillbox", task="price books", distiller=dl)
    got = {c.subject: c for c in s2.recall("discount pricing greedy grouping", project="pillbox", include_candidates=True)}
    assert got["book discount pricing"].status == "accepted", "type=decision auto-accepts"
    pit = got["greedy grouping overcharges"]
    assert pit.status == "candidate" and "glm-4.6" not in pit.content and "<model>" in pit.content, pit.content
    assert pit.code_refs and pit.code_refs[0]["path"] == "src/bookstore.py"
    print(f"OK — LLM distiller (stub): parsed {len(drafts)} drafts (fence+prose stripped, blank dropped, "
          f"bad type coerced), wrote {len(ids2)}; decision auto-accepted, pitfall model-agnostic + code-anchored")
