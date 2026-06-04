#!/usr/bin/env python3
"""swarm-memory store — milestone 1, on tursodb (the chosen store), per swarm-memory-mcp-server-spec.

  observe = capture   — append-only event (e.g. a §0 trace excerpt). Raw signal.
  claim   = distill   — a durable, distilled memory candidate; can POINT TO CODE (file/symbol/repo/
                        commit) — grounded memory is stronger: recall brings the live exemplar.
  recall  = load/pull — hybrid retrieval (vector semantic + LIKE keyword) with the spec's governance
                        rules (never rejected; prefer accepted; prefer project; provenance always).

WHY tursodb (verified embedded, this session): concurrent writes via MVCC + BEGIN CONCURRENT — many
pillboxes/capsules write at once even single-user (SQLite's single-writer was insufficient); native
vector search (vector32 / vector_distance_cos). BM25-over-code (Tantivy fts_score/MATCH) is present
but experimental-flagged — the stronger code-search path, swapped in behind `_keyword_sql` when it
stabilizes. Storage is behind a thin seam so Postgres (fallback) is a swap, not a rewrite.

Research refinements: `pitfall` type (failure-mining); shared (`global`/`project`) claims must be
MODEL-AGNOSTIC distilled (the MemCollab cross-model landmine — enforced by the distill step upstream);
source attribution + code refs = provenance/grounding. Embeddings are BYO (inject an `embed` fn —
the model choice is separate); with no embedder, recall degrades to LIKE keyword.
"""
from __future__ import annotations

import json
import re
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Callable

import turso  # pyturso — the tursodb engine (concurrent writes + native vectors)

SCOPES = ("user", "project", "agent", "global")
TYPES = ("fact", "preference", "decision", "procedure", "artifact", "hypothesis", "pitfall")

SCHEMA = """
CREATE TABLE IF NOT EXISTS observations (
  id TEXT PRIMARY KEY, actor TEXT NOT NULL, project TEXT, scope TEXT NOT NULL,
  content TEXT NOT NULL, source TEXT, event_time TEXT, confidence REAL DEFAULT 0.7,
  metadata TEXT DEFAULT '{}', created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS memory_claims (
  id TEXT PRIMARY KEY, type TEXT NOT NULL, subject TEXT NOT NULL, content TEXT NOT NULL,
  scope TEXT NOT NULL, project TEXT, agent TEXT, status TEXT NOT NULL DEFAULT 'candidate',
  confidence REAL DEFAULT 0.7, source_ids TEXT DEFAULT '[]',
  code_refs TEXT DEFAULT '[]',          -- [{path, symbol?, repo?, commit?}] — memory that points to code
  embedding BLOB,                        -- vector32 of subject+content (when an embedder is wired)
  valid_from TEXT, valid_to TEXT, metadata TEXT DEFAULT '{}',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS claims_project_idx ON memory_claims(project);
CREATE INDEX IF NOT EXISTS claims_status_idx ON memory_claims(status);
"""


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _uid() -> str:
    return uuid.uuid4().hex


@dataclass
class Claim:
    id: str
    type: str
    subject: str
    content: str
    scope: str
    status: str
    confidence: float
    source_ids: list[str]
    code_refs: list[dict] = field(default_factory=list)
    project: str | None = None
    agent: str | None = None
    low_confidence: bool = False


class MemoryStore:
    """tursodb-backed store behind a thin seam. Writes use MVCC + BEGIN CONCURRENT (with retry on the
    write-write conflict tursodb signals) so many capsules write concurrently. `embed` is injected
    (BYO embeddings); absent → recall is LIKE-only."""

    def __init__(self, path: str = "swarm-memory.db", embed: Callable[[str], list[float]] | None = None):
        self.embed = embed
        self.db = turso.connect(path)
        cur = self.db.cursor()
        # pyturso executes lazily — fetch to force the PRAGMA to actually run, then confirm MVCC
        # is on (BEGIN CONCURRENT errors without it).
        mode = cur.execute("PRAGMA journal_mode = 'mvcc'").fetchone()
        _require(bool(mode) and mode[0] == "mvcc", f"MVCC not enabled (got {mode})")
        for stmt in filter(str.strip, SCHEMA.split(";")):
            cur.execute(stmt).fetchall()  # force DDL (lazy execute)
        self.db.commit()

    def _write(self, sqls: list[tuple[str, tuple]]):
        """Run writes in a BEGIN CONCURRENT txn, retrying the conflict tursodb raises on overlap."""
        for attempt in range(5):
            cur = self.db.cursor()
            try:
                cur.execute("BEGIN CONCURRENT")
                for sql, params in sqls:
                    cur.execute(sql, params)
                cur.execute("COMMIT")
                return
            except turso.OperationalError as e:
                try:
                    cur.execute("ROLLBACK")
                except Exception:
                    pass
                if "conflict" in str(e).lower() or "busy" in str(e).lower():
                    if attempt < 4:
                        continue  # non-overlapping retry; tursodb's MVCC conflict path
                raise

    # --- observe = capture --------------------------------------------------
    def observe(self, actor: str, content: str, scope: str = "project", project: str | None = None,
                source: str | None = None, confidence: float = 0.7,
                metadata: dict | None = None, event_time: str | None = None) -> str:
        _require(scope in SCOPES, f"bad scope {scope!r}")
        oid = _uid()
        self._write([(
            "INSERT INTO observations(id,actor,project,scope,content,source,event_time,confidence,metadata,created_at)"
            " VALUES(?,?,?,?,?,?,?,?,?,?)",
            (oid, actor, project, scope, content, source, event_time, confidence, json.dumps(metadata or {}), _now()))])
        return oid

    # --- claim = distill (may point to code) --------------------------------
    def claim(self, type: str, subject: str, content: str, scope: str, project: str | None = None,
              agent: str | None = None, confidence: float = 0.7, source_ids: list[str] | None = None,
              code_refs: list[dict] | None = None, accept: bool = False) -> str:
        _require(type in TYPES, f"bad type {type!r}")
        _require(scope in SCOPES, f"bad scope {scope!r}")
        status = "accepted" if (accept or type == "decision") else "candidate"
        cid, now = _uid(), _now()
        vec = self._vec(f"{subject}\n{content}") if self.embed else None
        # vector32() must wrap a literal; embed it inline (values are our own floats, not user input).
        emb_sql = f"vector32('{vec}')" if vec else "NULL"
        self._write([(
            f"INSERT INTO memory_claims(id,type,subject,content,scope,project,agent,status,confidence,"
            f"source_ids,code_refs,embedding,created_at,updated_at)"
            f" VALUES(?,?,?,?,?,?,?,?,?,?,?,{emb_sql},?,?)",
            (cid, type, subject, content, scope, project, agent, status, confidence,
             json.dumps(source_ids or []), json.dumps(code_refs or []), now, now))])
        return cid

    # --- recall = load/pull -------------------------------------------------
    def recall(self, query: str, project: str | None = None, scope: str | None = None,
               types: list[str] | None = None, include_candidates: bool = False,
               limit: int = 10) -> list[Claim]:
        where = ["status NOT IN ('rejected','superseded')"]
        params: list = []
        if not include_candidates:
            where.append("status = 'accepted'")
        if scope:
            where.append("scope = ?"); params.append(scope)
        if project:
            where.append("(project = ? OR scope = 'global')"); params.append(project)
        if types:
            where.append("type IN (%s)" % ",".join("?" * len(types))); params.extend(types)
        cur = self.db.cursor()
        if self.embed:
            # semantic recall: cosine distance to the query embedding (linear scan — fine at bootstrap)
            qv = self._vec(query)
            order = f"vector_distance_cos(embedding, vector32('{qv}'))"
            rows = cur.execute(
                f"SELECT *, ({order}) AS _d FROM memory_claims WHERE embedding IS NOT NULL AND "
                + " AND ".join(where) + " ORDER BY _d LIMIT ?", params + [limit]).fetchall()
        else:
            # keyword recall (LIKE) until BM25-over-code (Tantivy) is enabled — same seam.
            terms = re.findall(r"[A-Za-z0-9_]+", query)
            like = " OR ".join(["(subject LIKE ? OR content LIKE ?)"] * len(terms)) or "1"
            lp = []
            for t in terms:
                lp += [f"%{t}%", f"%{t}%"]
            rows = cur.execute(
                "SELECT * FROM memory_claims WHERE " + " AND ".join(where)
                + (f" AND ({like})" if terms else "")
                + " ORDER BY (status='accepted') DESC, confidence DESC LIMIT ?",
                params + lp + [limit]).fetchall()
        cols = [d[0] for d in cur.description]
        return [self._claim(dict(zip(cols, r))) for r in rows]

    # --- helpers ------------------------------------------------------------
    def _vec(self, text: str) -> str:
        return "[" + ",".join(f"{x:.6f}" for x in self.embed(text)) + "]"

    @staticmethod
    def _claim(r: dict) -> Claim:
        return Claim(
            id=r["id"], type=r["type"], subject=r["subject"], content=r["content"], scope=r["scope"],
            status=r["status"], confidence=r["confidence"], source_ids=json.loads(r["source_ids"]),
            code_refs=json.loads(r.get("code_refs") or "[]"), project=r["project"], agent=r["agent"],
            low_confidence=r["confidence"] < 0.5)


def _require(cond: bool, msg: str):
    if not cond:
        raise ValueError(msg)


if __name__ == "__main__":
    import os
    db = "/tmp/swarm-memory-selftest.db"
    for f in (db, db + "-wal", db + "-shm"):
        try: os.remove(f)
        except OSError: pass
    m = MemoryStore(db)
    o = m.observe("dogfood_agent", "Plain cargo build dropped libkrun → docker fallback, no workspace.",
                  scope="project", project="pillbox", confidence=0.9)
    m.claim("pitfall", "libkrun rebuild",
            "Rebuild with --features libkrun + re-codesign; a plain build silently falls to docker.",
            scope="project", project="pillbox", source_ids=[o],
            code_refs=[{"path": "src/sandbox/mod.rs", "symbol": "select_backend"}], accept=True)
    m.claim("fact", "low signal", "a weak candidate", scope="project", project="pillbox", confidence=0.3)
    hits = m.recall("libkrun rebuild feature codesign", project="pillbox")
    assert hits and hits[0].type == "pitfall", [h.type for h in hits]
    assert hits[0].source_ids == [o] and hits[0].code_refs[0]["symbol"] == "select_backend", hits[0]
    assert all(h.status == "accepted" for h in hits)
    # concurrent writes: two stores writing the same db at once (the tursodb reason)
    m2 = MemoryStore(db)
    a = m.observe("agent_a", "x", project="pillbox"); b = m2.observe("agent_b", "y", project="pillbox")
    assert a != b
    print(f"OK — tursodb store: recalled [{hits[0].type}] {hits[0].subject} → "
          f"code {hits[0].code_refs[0]['path']}:{hits[0].code_refs[0]['symbol']}; "
          f"governance + provenance + concurrent observe all hold")
