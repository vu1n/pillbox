#!/usr/bin/env python3
"""arbiter.py — memory governance: consolidate near-duplicate claims, surface conflicts.

The dedup/supersede layer the dogfood proved necessary: 359 real sessions distilled to 275 claims,
many near-identical ("most criteria failed (8/8)" ×18). Per swarm-memory-mcp-server-spec milestone 3
+ the arbiter rules — group live claims by (subject, scope, project), keep the strongest, SUPERSEDE
the rest. Never deletes: superseded rows stay for history; recall already excludes them.

"Strongest" = accepted-over-candidate, then higher confidence, then more sources (evidence), then
newer. This also implements decision supersession (a newer decision on the same subject wins) that
`decide` deferred here. Read-only `resolve_conflicts` reports a subject's claims grouped by status.
"""
from __future__ import annotations

import os
import sys
from collections import defaultdict

from .store import Claim, MemoryStore, store_from_env


def _rank(c: Claim) -> tuple:
    """Sort key, higher = stronger survivor: accepted first, then confidence, evidence, recency."""
    return (c.status == "accepted", c.confidence or 0, len(c.source_ids), c.updated_at)


def _plan_groups(groups: list[list[Claim]]) -> list[dict]:
    """For each group of >1, keep the strongest (by _rank) and supersede the rest."""
    plan = []
    for members in groups:
        if len(members) < 2:
            continue
        survivor, *losers = sorted(members, key=_rank, reverse=True)
        plan.append({"subject": survivor.subject, "survivor": survivor.id,
                     "superseded": [m.id for m in losers]})
    return plan


def _semantic_clusters(pairs: list[tuple[str, str]], alive: dict[str, Claim]) -> list[list[Claim]]:
    """Union-find the near-dup pairs (restricted to still-alive ids) into clusters of >1."""
    parent: dict[str, str] = {}

    def find(x: str) -> str:
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    for a, b in pairs:
        if a in alive and b in alive:
            parent[find(a)] = find(b)
    clusters: dict[str, list[Claim]] = defaultdict(list)
    for cid in parent:
        clusters[find(cid)].append(alive[cid])
    return [c for c in clusters.values() if len(c) > 1]


def consolidate(store, *, project: str | None = None, subject: str | None = None,
                dry_run: bool = False, semantic: float | None = None) -> dict:
    """Phase 1: group live claims by exact (subject, scope, project); keep the strongest, supersede the
    rest. Phase 2 (when `semantic` is a cosine max-distance AND claims are embedded): cluster the
    SURVIVORS' different-subject near-duplicates and dedup those too — the LLM-distiller case, where
    each lesson gets a distinct subject but many mean the same thing. dry_run returns the plan without
    writing. Returns {groups, superseded, dry_run, plan:[{subject, survivor, superseded:[ids]}]}."""
    claims = store.live_claims(project, subject)
    by_subject: dict[tuple, list[Claim]] = defaultdict(list)
    for c in claims:
        by_subject[(c.subject, c.scope, c.project)].append(c)
    plan = _plan_groups(list(by_subject.values()))
    superseded = {cid for p in plan for cid in p["superseded"]}

    if semantic is not None:
        alive = {c.id: c for c in claims if c.id not in superseded}
        sem_plan = _plan_groups(_semantic_clusters(store.similar_pairs(project, max_distance=semantic), alive))
        plan += sem_plan
        superseded |= {cid for p in sem_plan for cid in p["superseded"]}

    if not dry_run:
        for cid in superseded:
            store.set_status(cid, "superseded")

    return {"groups": len(plan), "superseded": len(superseded), "dry_run": dry_run, "plan": plan}


def resolve_conflicts(store, subject: str, *, project: str | None = None) -> dict:
    """Read-only: a subject's live claims grouped by status, with the recommended survivor. Apply via
    consolidate."""
    members = store.live_claims(project, subject)
    by_status: dict[str, list] = defaultdict(list)
    for m in members:
        by_status[m.status].append({"id": m.id, "content": m.content,
                                     "confidence": m.confidence, "updated_at": m.updated_at})
    return {"subject": subject, "count": len(members), "by_status": dict(by_status),
            "recommend": max(members, key=_rank).id if members else None}


def main():
    import argparse

    ap = argparse.ArgumentParser(description="consolidate near-duplicate claims in the memory store")
    ap.add_argument("--project", default=os.environ.get("GHOST_PROJECT", "default"))
    ap.add_argument("--subject", help="limit to one subject (default: whole project)")
    ap.add_argument("--dry-run", action="store_true", help="show the plan, write nothing")
    ap.add_argument("--semantic", type=float, default=None, metavar="DIST",
                    help="also merge different-subject near-dups within this cosine distance — "
                         "calibrate per embedder (~0.25 for nomic-embed-text); needs GHOST_EMBED_MODEL")
    args = ap.parse_args()

    result = consolidate(store_from_env(), project=args.project, subject=args.subject,
                         dry_run=args.dry_run, semantic=args.semantic)
    verb = "would supersede" if args.dry_run else "superseded"
    print(f"{result['groups']} duplicate group(s); {verb} {result['superseded']} claim(s) "
          f"in project {args.project!r}")
    for p in result["plan"][:20]:
        print(f"  keep {p['survivor'][:8]} · drop {len(p['superseded'])} — {p['subject']}")


if __name__ == "__main__" and len(sys.argv) > 1:
    main()
elif __name__ == "__main__":
    # self-test: seed a duplicated subject + a conflicting decision, consolidate, assert dedup.
    import glob

    db = "/tmp/arbiter-selftest.db"
    for f in glob.glob(db + "*"):
        try: os.remove(f)
        except OSError: pass
    store = MemoryStore(db)

    # three claims, same subject — varying strength (accepted > high-conf > low-conf+more-sources)
    weak = store.claim("pitfall", "build fails", "weak", scope="project", project="p", confidence=0.3)
    mid = store.claim("pitfall", "build fails", "mid, more evidence", scope="project", project="p",
                      confidence=0.5, source_ids=["o1", "o2"])
    strong = store.claim("pitfall", "build fails", "accepted truth", scope="project", project="p",
                         confidence=0.4, accept=True)
    other = store.claim("fact", "unrelated", "kept", scope="project", project="p", accept=True)

    # dry-run writes nothing
    dry = consolidate(store, project="p", dry_run=True)
    assert dry["groups"] == 1 and dry["superseded"] == 2, dry
    assert len(store.recall("build fails", project="p", include_candidates=True)) == 3, "dry-run mutated"

    rc = resolve_conflicts(store, "build fails", project="p")
    assert rc["count"] == 3 and rc["recommend"] == strong, rc  # accepted wins the recommendation

    # apply: the accepted claim survives, the other two are superseded (gone from recall)
    res = consolidate(store, project="p")
    assert res["superseded"] == 2 and res["plan"][0]["survivor"] == strong, res
    live = store.recall("build fails", project="p", include_candidates=True)
    assert [c.id for c in live] == [strong], [c.id for c in live]
    assert any(c.subject == "unrelated" for c in store.recall("unrelated", project="p")), "singleton untouched"
    # idempotent: a second pass finds nothing to do
    assert consolidate(store, project="p")["superseded"] == 0, "should be idempotent"
    assert weak and mid and other  # (silence unused-var lint; ids exercised via recall)

    print(f"OK — arbiter: consolidated 3 dupes → 1 survivor ({strong[:8]}, the accepted claim); "
          f"2 superseded (history kept, recall excludes); singleton untouched; idempotent")

    # semantic dedup: DIFFERENT subjects, ~identical embeddings → merged (the LLM-distiller case).
    db2 = "/tmp/arbiter-sem-selftest.db"
    for f in glob.glob(db2 + "*"):
        try: os.remove(f)
        except OSError: pass

    def toy(text):  # bag-of-keywords: the two "python interpreter" claims land on the same vector
        t = text.lower()
        return [float("python" in t), float("interpret" in t or "execut" in t), float("test" in t), 1.0]

    s2 = MemoryStore(db2, embed=toy)
    s2.claim("procedure", "Python interpreter execution", "run the python interpreter to execute",
             scope="project", project="p", accept=True)
    s2.claim("procedure", "Python interpreter availability", "check the python interpreter is present",
             scope="project", project="p", confidence=0.5)
    s2.claim("pitfall", "Domino chain validation", "validate the domino chain endpoints match",
             scope="project", project="p", accept=True)
    assert consolidate(s2, project="p", dry_run=True)["superseded"] == 0, "exact pass: subjects all distinct"
    res2 = consolidate(s2, project="p", semantic=0.05)
    live2 = {c.subject for c in s2.live_claims("p")}
    assert res2["superseded"] == 1 and "Domino chain validation" in live2, (res2, live2)
    assert sum("Python" in s for s in live2) == 1, live2  # the two near-dup subjects → one survivor
    print(f"OK — arbiter semantic: 2 different-subject near-dups merged via embeddings → "
          f"{sorted(live2)}; the distinct claim untouched")
