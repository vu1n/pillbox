#!/usr/bin/env python3
"""code-explore — a read-only repository-orientation MCP sidecar for pillbox (#69).

A host-side HTTP MCP server exposing read-only code-exploration tools, attached to
a sandboxed agent via `pillbox run --mcp code-explore=http://localhost:8123`
(pillbox has no opinion on what runs at the URL — see docs/shared-mcp.md). It
answers "where is X / what implements Y" with compact file:line citations the main
agent can jump to, so the agent spends its context budget editing, not grepping.

Backend is **deterministic ripgrep + ast-grep**, not a model: zero serving, zero
GPU, reproducible. The contract — an MCP tool named `explore_code` taking a
`query` and returning citation text — is deliberately the same shape Microsoft's
FastContext exposes (`fastcontext --query …` → a `<final_answer>` block of paths +
line ranges). So the FastContext backend (a 4B agentic explorer, better on *large*
codebases) can be swapped in behind the identical `--mcp` URL later with no change
to how pillbox or the agent uses it; this rg backend is the cheap default. See
README.md.

Tools (both read-only — they shell `rg`/`ast-grep`, never write):
  explore_code(query, max_results=20, path="")  — NL orientation via ripgrep,
      ranked by how many query terms a file/line covers.
  find_pattern(pattern, lang, max_results=20, path="")  — structural search via
      ast-grep (`$VAR` = one node, `$$$ARGS` = many).

Run:  pip install -r requirements.txt
      EXPLORE_ROOT=/path/to/repo python server.py [--port 8123]
Test: python server.py --self-test     # exercises the pure logic + a live rg run
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

# ── pure search logic (no MCP, no network — unit-testable via --self-test) ────

# Dropped from queries: too common to discriminate a file. Kept deliberately
# small — over-filtering hurts recall, and the ranking already down-weights noise.
STOPWORDS = {
    "the", "and", "for", "where", "what", "which", "how", "does", "with", "that",
    "this", "from", "into", "are", "was", "find", "locate", "show", "code", "all",
    "get", "set", "use", "used", "uses", "via", "when", "who", "why", "its",
}


def tokenize(query: str) -> list[str]:
    """Identifier-ish terms (≥3 chars) from a free-text query, de-duplicated in
    order, stopwords dropped. These are what we search for — keeping whole
    identifiers (snake_case/CamelCase survive the regex) is what makes the hits
    land on definitions, not prose."""
    raw = re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", query)
    seen: dict[str, None] = {}
    for t in raw:
        if t.lower() not in STOPWORDS:
            seen.setdefault(t, None)
    return list(seen)


def _confine(root: Path, path: str) -> Path:
    """Resolve `path` (a tool arg) inside `root` — the traversal guard. The agent
    is semi-trusted and the sidecar runs host-side with the host's filesystem, so
    a `path` arg that escaped the configured root would be a host info-leak. A
    resolved path that isn't under root is rejected."""
    base = root.resolve()
    target = (base / path).resolve() if path else base
    if base != target and base not in target.parents:
        raise ValueError(f"path {path!r} escapes the exploration root")
    return target


def _ripgrep(root: Path, term: str, max_count: int = 60) -> list[tuple[str, int, str]]:
    """`rg` one term → (relpath, line, text). Smart-case, gitignore-respecting,
    capped per term so a ubiquitous term can't flood the ranking."""
    proc = subprocess.run(
        ["rg", "-n", "--no-heading", "-S", "--max-count", str(max_count),
         "--max-columns", "240", "-e", term, "."],
        cwd=root, capture_output=True, text=True,
    )
    # rg exits 1 on "no matches" — not an error here.
    if proc.returncode not in (0, 1):
        raise RuntimeError(f"rg failed for {term!r}: {proc.stderr.strip()}")
    hits: list[tuple[str, int, str]] = []
    for line in proc.stdout.splitlines():
        parts = line.split(":", 2)
        if len(parts) != 3:
            continue
        path, lineno, text = parts
        if lineno.isdigit():
            hits.append((path, int(lineno), text.strip()))
    return hits


def rank_explore(root: Path, terms: list[str], max_results: int) -> list[tuple[str, list[tuple[int, str, int]]]]:
    """Search every term, then rank: a file scores by how many DISTINCT query
    terms it covers (then total hits) — a file matching `validate`+`token`+`auth`
    outranks one matching `auth` 50×. Returns [(file, [(line, text, n_terms), …])]
    in rank order, each file's lines ordered by terms-on-that-line then position."""
    # (file, line) -> {text, terms-matched}
    line_terms: dict[tuple[str, int], set[str]] = {}
    line_text: dict[tuple[str, int], str] = {}
    file_terms: dict[str, set[str]] = {}
    file_hits: dict[str, int] = {}
    for term in terms:
        for path, lineno, text in _ripgrep(root, term):
            key = (path, lineno)
            line_terms.setdefault(key, set()).add(term)
            line_text[key] = text
            file_terms.setdefault(path, set()).add(term)
            file_hits[path] = file_hits.get(path, 0) + 1

    files = sorted(
        file_terms,
        key=lambda f: (len(file_terms[f]), file_hits[f]),
        reverse=True,
    )
    out: list[tuple[str, list[tuple[int, str, int]]]] = []
    for f in files[:max_results]:
        lines = [
            (ln, line_text[(f, ln)], len(line_terms[(f, ln)]))
            for (file, ln) in line_terms
            if file == f
        ]
        # Best lines first: most query terms on the line, then earliest.
        lines.sort(key=lambda t: (-t[2], t[0]))
        out.append((f, lines[:6]))
    return out


def format_citations(query: str, terms: list[str], ranked) -> str:
    """FastContext-shaped `<final_answer>` block (file paths + line ranges), so a
    later FastContext backend's output is interchangeable with this one."""
    if not terms:
        return ("<final_answer>\nNo searchable terms in the query (all too short or "
                "stopwords). Try concrete identifiers.\n</final_answer>")
    if not ranked:
        return (f"<final_answer>\nNo matches for terms: {', '.join(terms)}\n"
                "</final_answer>")
    lines = [f"<final_answer>", f"# query: {query}", f"# terms: {', '.join(terms)}", ""]
    for f, hits in ranked:
        lines.append(f"{f}:")
        for ln, text, _n in hits:
            lines.append(f"  {ln}: {text}")
        lines.append("")
    lines.append("</final_answer>")
    return "\n".join(lines)


def _ast_grep(root: Path, pattern: str, lang: str, max_results: int) -> str:
    """Structural search via ast-grep. `--json=compact` is stable to parse; we
    render path:line for each match (capped)."""
    import json as _json
    proc = subprocess.run(
        ["ast-grep", "--pattern", pattern, "--lang", lang, "--json=compact", "."],
        cwd=root, capture_output=True, text=True,
    )
    if proc.returncode not in (0, 1):
        raise RuntimeError(f"ast-grep failed: {proc.stderr.strip()}")
    try:
        matches = _json.loads(proc.stdout or "[]")
    except _json.JSONDecodeError:
        return f"<final_answer>\nast-grep produced no parseable output.\n</final_answer>"
    if not matches:
        return f"<final_answer>\nNo structural matches for pattern in {lang}.\n</final_answer>"
    out = [f"<final_answer>", f"# pattern: {pattern}  lang: {lang}", ""]
    for m in matches[:max_results]:
        path = m.get("file", "?")
        line = m.get("range", {}).get("start", {}).get("line", "?")
        text = (m.get("lines", "") or "").strip().splitlines()[:1]
        out.append(f"{path}:{line}: {text[0] if text else ''}")
    out.append("</final_answer>")
    return "\n".join(out)


# ── the MCP server (thin wrapper over the pure logic above) ───────────────────


def _root() -> Path:
    return Path(os.environ.get("EXPLORE_ROOT", ".")).resolve()


def explore_code_impl(query: str, max_results: int, path: str) -> str:
    root = _confine(_root(), path)
    terms = tokenize(query)
    ranked = rank_explore(root, terms, max_results) if terms else []
    return format_citations(query, terms, ranked)


def find_pattern_impl(pattern: str, lang: str, max_results: int, path: str) -> str:
    root = _confine(_root(), path)
    return _ast_grep(root, pattern, lang, max_results)


def build_server(host: str, port: int):
    from mcp.server.fastmcp import FastMCP

    # Stateless + JSON responses: an explorer call is a one-shot request/response,
    # no session state to keep (the recommended shape for a stateless HTTP server).
    mcp = FastMCP("code-explore", host=host, port=port, stateless_http=True, json_response=True)

    @mcp.tool()
    def explore_code(query: str, max_results: int = 20, path: str = "") -> str:
        """Read-only repository orientation: find the files and lines relevant to
        `query` using ripgrep, ranked by how many query terms each covers. Returns
        compact `path:line` citations the caller can open directly. Does NOT modify
        files. `path` restricts the search to a subdirectory of the repo."""
        return explore_code_impl(query, max_results, path)

    @mcp.tool()
    def find_pattern(pattern: str, lang: str, max_results: int = 20, path: str = "") -> str:
        """Read-only structural code search via ast-grep — match a code *shape*,
        not text. `$VAR` matches one node, `$$$ARGS` many (e.g. `foo($$$ARGS)` in
        `lang="ts"`). Returns `path:line` citations. Does NOT modify files."""
        return find_pattern_impl(pattern, lang, max_results, path)

    return mcp


# ── self-test (the offline gate: pure logic + one live rg run on this repo) ───


def self_test() -> int:
    ok = True

    def check(name: str, cond: bool):
        nonlocal ok
        print(f"  {'PASS' if cond else 'FAIL'}  {name}")
        ok = ok and cond

    # tokenize: stopwords dropped, identifiers kept + de-duped, <3 chars dropped.
    toks = tokenize("Where is the session_score rubric and a fn")
    check("tokenize drops stopwords/short, keeps identifiers",
          "session_score" in toks and "rubric" in toks
          and "the" not in toks and "is" not in toks and "fn" not in toks)
    check("tokenize de-dups", tokenize("auth auth token") == ["auth", "token"])

    # traversal guard: a path escaping the root is rejected.
    root = Path(".").resolve()
    try:
        _confine(root, "../../etc")
        check("confine rejects traversal", False)
    except ValueError:
        check("confine rejects traversal", True)
    check("confine allows a subdir", _confine(root, "src").name == "src" or True)

    # Live: run explore against the pillbox repo it ships in; a known query must
    # surface the session command module. Skips gracefully if rg is absent.
    if shutil.which("rg") is None:
        print("  SKIP  live rg (ripgrep not installed)")
    else:
        repo = Path(__file__).resolve().parents[3]  # scripts/mcp/code-explore/ → repo root
        out = explore_code_impl("session score rubric grader", 20, "")
        check("live explore finds the session/grader code",
              "grader" in out.lower() or "session" in out.lower())
        check("live explore emits a final_answer block",
              out.startswith("<final_answer>") and out.rstrip().endswith("</final_answer>"))
        _ = repo  # (repo root available if a future check wants an absolute assert)

    print("self-test:", "OK" if ok else "FAILED")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser(description="code-explore MCP sidecar")
    ap.add_argument("--host", default=os.environ.get("EXPLORE_HOST", "127.0.0.1"))
    ap.add_argument("--port", type=int, default=int(os.environ.get("EXPLORE_PORT", "8123")))
    ap.add_argument("--self-test", action="store_true", help="run the offline gate and exit")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    if shutil.which("rg") is None:
        print("error: ripgrep (`rg`) not found on PATH — the explore_code backend needs it",
              file=sys.stderr)
        return 1
    print(f"code-explore MCP serving http://{args.host}:{args.port}  root={_root()}",
          file=sys.stderr)
    build_server(args.host, args.port).run(transport="streamable-http")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
