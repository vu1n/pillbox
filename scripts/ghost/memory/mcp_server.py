#!/usr/bin/env python3
"""mcp_server.py — the swarm-memory engine as a single MCP server (observe / claim / recall + the
spec's decide / remember_procedure conveniences).

The ONE optional MCP an agent attaches (per swarm-memory-mcp-server-spec): a thin semantic layer over
the store — the product is memory governance, not the transport. The server is bound to one project
and one db file; many agents/pillboxes run their own server against the SAME db (tursodb concurrent
writes). Code grounding is wired by default via RipgrepResolver, so recall returns live code pointers
with zero setup; an embedder (vector recall) and a canopy/AST resolver drop in behind store's seams.

Layering for testability: the tool LOGIC is in plain `do_*` functions (take a store, no `mcp` dep —
the self-test exercises these directly); `build_mcp` is thin registration that imports the MCP SDK
lazily. So this module imports and self-tests without the SDK; the SDK is only needed to serve.
"""
from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from store import MemoryStore, RipgrepResolver  # noqa: E402 — sibling import, path set above


def _claim_dict(c) -> dict:
    """Claim → the JSON an agent consumes. Includes resolved code `grounding` (live pointers) and the
    `low_confidence` flag the spec wants surfaced."""
    return {"id": c.id, "type": c.type, "subject": c.subject, "content": c.content, "scope": c.scope,
            "status": c.status, "confidence": c.confidence, "source_ids": c.source_ids,
            "code_refs": c.code_refs, "grounding": c.grounding, "low_confidence": c.low_confidence}


def do_observe(store, content: str, *, project: str, actor: str = "agent", scope: str = "project",
               source: str | None = None, confidence: float = 0.7) -> str:
    return store.observe(actor=actor, content=content, scope=scope, project=project,
                         source=source, confidence=confidence)


def do_claim(store, *, type: str, subject: str, content: str, project: str, scope: str = "project",
             confidence: float = 0.7, source_ids: list[str] | None = None,
             code_refs: list[dict] | None = None, accept: bool = False) -> str:
    return store.claim(type, subject, content, scope=scope, project=project, confidence=confidence,
                       source_ids=source_ids, code_refs=code_refs, accept=accept)


def do_recall(store, query: str, *, project: str, scope: str | None = None,
              types: list[str] | None = None, include_candidates: bool = False,
              limit: int = 10) -> list[dict]:
    return [_claim_dict(c) for c in store.recall(query, project=project, scope=scope, types=types,
                                                 include_candidates=include_candidates, limit=limit)]


def do_decide(store, subject: str, content: str, *, project: str, source_ids: list[str] | None = None) -> str:
    # type=decision auto-accepts in the store. (Superseding prior decisions on the same subject is the
    # consolidate/arbiter slice — spec milestone 2 — not done here.)
    return store.claim("decision", subject, content, scope="project", project=project, source_ids=source_ids)


def do_remember_procedure(store, subject: str, content: str, *, project: str,
                          source_ids: list[str] | None = None) -> str:
    return store.claim("procedure", subject, content, scope="project", project=project,
                       source_ids=source_ids, accept=True)


def build_mcp(store, project: str, *, name: str = "ghost-memory"):
    """Register the engine's tools on a FastMCP server bound to `store`/`project`. Imports the MCP SDK
    lazily so the module stays importable (and testable) without it. Docstrings/type hints below are
    the agent-facing tool contract."""
    from mcp.server.fastmcp import FastMCP

    mcp = FastMCP(name)

    @mcp.tool()
    def observe(content: str, actor: str = "agent", source: str = "", scope: str = "project") -> str:
        """Record a raw observation — an append-only signal (a finding, an error seen, a choice made in
        passing). Returns the observation id; pass it as a claim's source_id to attribute provenance."""
        return do_observe(store, content, project=project, actor=actor, scope=scope, source=source or None)

    @mcp.tool()
    def claim(subject: str, content: str, type: str = "fact", confidence: float = 0.7,
              scope: str = "project", source_ids: list[str] | None = None,
              code_refs: list[dict] | None = None) -> str:
        """Record a durable memory CANDIDATE (type: fact|preference|decision|procedure|artifact|
        hypothesis|pitfall). Memory is shared across the swarm — keep content MODEL-AGNOSTIC (no model
        names). Anchor to code via code_refs [{symbol,path,query}] when it concerns specific code.
        Returns the claim id."""
        return do_claim(store, type=type, subject=subject, content=content, project=project,
                        scope=scope, confidence=confidence, source_ids=source_ids, code_refs=code_refs)

    @mcp.tool()
    def recall(query: str, scope: str = "", types: list[str] | None = None,
               include_candidates: bool = False, limit: int = 10) -> list[dict]:
        """Recall relevant memory for a query (semantic if an embedder is wired, else keyword). Prefers
        accepted over candidate, project over global, never returns rejected. Each result carries live
        code `grounding` (resolved against the current tree) and a `low_confidence` flag."""
        return do_recall(store, query, project=project, scope=scope or None, types=types,
                         include_candidates=include_candidates, limit=limit)

    @mcp.tool()
    def decide(subject: str, content: str, source_ids: list[str] | None = None) -> str:
        """Record an ACCEPTED decision (durable, not a candidate) — the team's chosen answer for a
        subject. Returns the claim id."""
        return do_decide(store, subject, content, project=project, source_ids=source_ids)

    @mcp.tool()
    def remember_procedure(subject: str, content: str, source_ids: list[str] | None = None) -> str:
        """Record an ACCEPTED procedure — a reusable how-to. Returns the claim id."""
        return do_remember_procedure(store, subject, content, project=project, source_ids=source_ids)

    return mcp


def main():
    db = os.environ.get("GHOST_MEMORY_DB", os.path.expanduser("~/.pillbox/ghost/swarm-memory.db"))
    project = os.environ.get("GHOST_PROJECT", "default")
    root = os.environ.get("GHOST_REPO_ROOT", ".")
    os.makedirs(os.path.dirname(db), exist_ok=True)
    store = MemoryStore(db, resolver=RipgrepResolver(root=root))
    build_mcp(store, project).run()  # FastMCP default transport: stdio


if __name__ == "__main__" and os.environ.get("GHOST_MCP_SERVE"):
    main()
elif __name__ == "__main__":
    # self-test: exercise the do_* logic directly (the MCP layer is thin glue over these), no SDK
    # needed. Set GHOST_MCP_SERVE=1 to actually run the stdio server instead.
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

    oid = do_observe(store, "agent hit a silent docker fallback", project="pillbox")
    cid = do_claim(store, type="pitfall", subject="libkrun rebuild",
                   content="rebuild with --features libkrun + re-codesign or it falls back to docker",
                   project="pillbox", source_ids=[oid],
                   code_refs=[{"symbol": "select_backend", "path": "src/sandbox/mod.rs"}], accept=True)
    did = do_decide(store, "store engine", "tursodb embedded — concurrent writes + portability", project="pillbox")
    pid = do_remember_procedure(store, "rebuild for libkrun", "cargo build --features libkrun; re-codesign", project="pillbox")
    assert oid and cid and did and pid

    hits = do_recall(store, "libkrun rebuild docker fallback", project="pillbox")
    top = hits[0]
    assert top["subject"] == "libkrun rebuild", hits
    assert top["status"] == "accepted" and top["source_ids"] == [oid]
    assert top["grounding"] and top["grounding"][0]["status"] == "grounded" \
        and top["grounding"][0]["location"]["line"] == 1, top["grounding"]
    decided = do_recall(store, "store engine tursodb", project="pillbox")
    assert any(h["type"] == "decision" and h["status"] == "accepted" for h in decided), decided
    proc = do_recall(store, "rebuild libkrun procedure", project="pillbox", types=["procedure"])
    assert any(h["type"] == "procedure" and h["status"] == "accepted" for h in proc), proc

    note = ""
    try:
        srv = build_mcp(store, "pillbox")
        tm = getattr(srv, "_tool_manager", None)
        names = sorted(getattr(tm, "_tools", {})) if tm else []
        note = f"; mcp server built (tools: {names or 'registered'})"
    except ImportError:
        note = "; mcp SDK not installed — server build skipped (do_* logic verified)"

    print(f"OK — mcp wrapper: observe/claim/decide/remember_procedure/recall all hold; "
          f"recall returns code grounding + status governance{note}")
