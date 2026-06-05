#!/usr/bin/env python3
"""mcp_server.py — the swarm-memory engine as a single MCP server (observe / claim / recall + the
spec's decide / remember_procedure conveniences).

The ONE optional MCP an agent attaches (per swarm-memory-mcp-server-spec): a thin semantic layer over
the store — the product is memory governance, not the transport. The server is bound to one project
and one db file; many agents/pillboxes run their own server against the SAME db (tursodb concurrent
writes). Code grounding is wired by default via RipgrepResolver, so recall returns live code pointers
with zero setup; an embedder (vector recall) and a canopy/AST resolver drop in behind store's seams.

The tools are thin closures over the store — transport + `_claim_dict` serialization, nothing more.
`build_mcp` imports the MCP SDK lazily, so this module imports and self-tests without the SDK; only
serving needs it.
"""
from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from arbiter import consolidate as _consolidate  # noqa: E402 — sibling, path set above
from arbiter import resolve_conflicts as _resolve_conflicts  # noqa: E402
from store import MemoryStore, RipgrepResolver, store_from_env  # noqa: E402


def _claim_dict(c) -> dict:
    """Claim → the JSON an agent consumes. Includes resolved code `grounding` (live pointers) and the
    `low_confidence` flag the spec wants surfaced."""
    return {"id": c.id, "type": c.type, "subject": c.subject, "content": c.content, "scope": c.scope,
            "status": c.status, "confidence": c.confidence, "source_ids": c.source_ids,
            "code_refs": c.code_refs, "grounding": c.grounding, "low_confidence": c.low_confidence}


def build_mcp(store, project: str, *, name: str = "ghost-memory"):
    """Register the engine's tools on a FastMCP server bound to `store`/`project`. Imports the MCP SDK
    lazily so the module stays importable (and testable) without it. The tool docstrings/type hints
    below are the agent-facing contract; each body is a thin call into the store."""
    from mcp.server.fastmcp import FastMCP

    mcp = FastMCP(name)

    @mcp.tool()
    def observe(content: str, actor: str = "agent", source: str = "", scope: str = "project") -> str:
        """Record a raw observation — an append-only signal (a finding, an error seen, a choice made in
        passing). Returns the observation id; pass it as a claim's source_id to attribute provenance."""
        return store.observe(actor=actor, content=content, scope=scope, project=project, source=source or None)

    @mcp.tool()
    def claim(subject: str, content: str, type: str = "fact", confidence: float = 0.7,
              scope: str = "project", source_ids: list[str] | None = None,
              code_refs: list[dict] | None = None) -> str:
        """Record a durable memory CANDIDATE (type: fact|preference|decision|procedure|artifact|
        hypothesis|pitfall). Memory is shared across the swarm — keep content MODEL-AGNOSTIC (no model
        names; the store does not strip them on this path). Anchor to code via code_refs
        [{symbol,path,query}] when it concerns specific code. Returns the claim id."""
        return store.claim(type, subject, content, scope=scope, project=project,
                           confidence=confidence, source_ids=source_ids, code_refs=code_refs)

    @mcp.tool()
    def recall(query: str, scope: str = "", types: list[str] | None = None,
               include_candidates: bool = False, limit: int = 10) -> list[dict]:
        """Recall relevant memory for a query (semantic if an embedder is wired, else keyword). Prefers
        accepted over candidate, project over global, never returns rejected. Each result carries live
        code `grounding` (resolved against the current tree) and a `low_confidence` flag."""
        return [_claim_dict(c) for c in store.recall(query, project=project, scope=scope or None,
                                                     types=types, include_candidates=include_candidates,
                                                     limit=limit)]

    @mcp.tool()
    def decide(subject: str, content: str, source_ids: list[str] | None = None) -> str:
        """Record an ACCEPTED decision (durable, not a candidate) — the team's chosen answer for a
        subject. Returns the claim id."""
        # type=decision auto-accepts in the store (the rule lives there, not restated here).
        return store.claim("decision", subject, content, scope="project", project=project, source_ids=source_ids)

    @mcp.tool()
    def remember_procedure(subject: str, content: str, source_ids: list[str] | None = None) -> str:
        """Record an ACCEPTED procedure — a reusable how-to. Returns the claim id."""
        return store.claim("procedure", subject, content, scope="project", project=project,
                           source_ids=source_ids, accept=True)

    @mcp.tool()
    def consolidate(subject: str = "", dry_run: bool = False, semantic: float = 0.0) -> dict:
        """Dedup memory: group claims by subject and SUPERSEDE all but the strongest (accepted >
        confident > more-evidenced > newer). subject="" consolidates the whole project; dry_run=true
        previews the plan without writing. semantic>0 (a cosine distance — calibrate per embedder,
        ~0.25 for nomic-embed-text) ALSO merges different-subject near-duplicates (needs an embedder).
        Superseded claims are kept for history but excluded from recall."""
        return _consolidate(store, project=project, subject=subject or None, dry_run=dry_run,
                            semantic=semantic or None)

    @mcp.tool()
    def resolve_conflicts(subject: str) -> dict:
        """Show a subject's live claims grouped by status, with the recommended survivor (read-only;
        apply the recommendation with consolidate)."""
        return _resolve_conflicts(store, subject, project=project)

    return mcp


def main():
    project = os.environ.get("GHOST_PROJECT", "default")
    build_mcp(store_from_env(), project).run()  # FastMCP default transport: stdio


if __name__ == "__main__" and os.environ.get("GHOST_MCP_SERVE"):
    main()
elif __name__ == "__main__":
    # self-test: the store + _claim_dict serialization (the MCP layer is thin closures over these),
    # no SDK needed. Set GHOST_MCP_SERVE=1 to run the stdio server instead.
    import glob
    import shutil

    db = "/tmp/ghost-mcp-selftest.db"
    for f in glob.glob(db + "*"):
        try: os.remove(f)
        except OSError: pass
    repo = "/tmp/ghost-mcp-selftest-repo"
    shutil.rmtree(repo, ignore_errors=True)
    os.makedirs(os.path.join(repo, "src", "sandbox"))
    with open(os.path.join(repo, "src", "sandbox", "mod.rs"), "w") as fh:
        fh.write("fn select_backend(cfg: &Cfg) -> Backend {\n    Backend::Libkrun\n}\n")

    store = MemoryStore(db, resolver=RipgrepResolver(root=repo))

    oid = store.observe(actor="agent", content="agent hit a silent docker fallback", scope="project", project="pillbox")
    store.claim("pitfall", "libkrun rebuild",
                "rebuild with --features libkrun + re-codesign or it falls back to docker",
                scope="project", project="pillbox", source_ids=[oid],
                code_refs=[{"symbol": "select_backend", "path": "src/sandbox/mod.rs"}], accept=True)
    # the presets the decide / remember_procedure tools apply: type=decision auto-accepts; procedure
    # is accepted via accept=True. Assert both land accepted (the tool closures rely on this).
    store.claim("decision", "store engine", "tursodb embedded — concurrent writes + portability",
                scope="project", project="pillbox")
    store.claim("procedure", "rebuild for libkrun", "cargo build --features libkrun; re-codesign",
                scope="project", project="pillbox", accept=True)

    hits = [_claim_dict(c) for c in store.recall("libkrun rebuild docker fallback", project="pillbox")]
    top = hits[0]
    assert top["subject"] == "libkrun rebuild", hits
    assert set(top) == {"id", "type", "subject", "content", "scope", "status", "confidence",
                        "source_ids", "code_refs", "grounding", "low_confidence"}, set(top)
    assert top["status"] == "accepted" and top["source_ids"] == [oid]
    assert top["grounding"] and top["grounding"][0]["status"] == "grounded" \
        and top["grounding"][0]["location"]["line"] == 1, top["grounding"]
    decided = [_claim_dict(c) for c in store.recall("store engine tursodb", project="pillbox")]
    assert any(h["type"] == "decision" and h["status"] == "accepted" for h in decided), decided
    proc = [_claim_dict(c) for c in store.recall("rebuild libkrun procedure", project="pillbox", types=["procedure"])]
    assert any(h["type"] == "procedure" and h["status"] == "accepted" for h in proc), proc

    note = ""
    try:
        srv = build_mcp(store, "pillbox")
        tm = getattr(srv, "_tool_manager", None)
        names = sorted(getattr(tm, "_tools", {})) if tm else []
        note = f"; mcp server built (tools: {names or 'registered'})"
    except ImportError:
        note = "; mcp SDK not installed — server build skipped (store + _claim_dict verified)"

    print(f"OK — mcp wrapper: tools are thin closures over the store; recall serializes via "
          f"_claim_dict with code grounding + status governance{note}")
