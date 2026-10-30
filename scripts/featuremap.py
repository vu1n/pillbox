#!/usr/bin/env python3
# vendored from vu1n/dev@065e973785d468d06812f0fc60c74b09d5915bb3; sha256 9a5862396bfe1a5e9a392d322e7a94cc7ca337c227e806bf9944b34be3343043
"""A repo's feature map: which code and tests implement each feature, and whether the map's
pointers still resolve.

The map is a directory, `docs/features/` by default: a `README.md` index plus one file per
feature. Each feature file opens with frontmatter:

    ---
    id: search
    paths:            # globs whose change means the feature needs re-driving
      - "src/search/**"
    entry:            # where to start reading: a file, or file:symbol
      - "src/search/index.ts:searchNotes"
    tests:            # functional tests that show what it does: a file, or file:test name
      - "test/search.test.ts"
      - "test/search.test.ts:returns title and body matches"
    ---

    features [--base REF] [FILE ...]   the features a change touches, with their tests and Gotchas,
                                       and whether their paths changed since their file last did
    check                              exit 1 when a pointer no longer resolves

A feature a member sees can add a `## User flow`: a ```mermaid flowchart of their path, drawn as a
statechart, then a proof table with one row per node, `| `id` | label | proof |`. The label is ✓ (a
test or a drive asserts it: the proof names a `file:test name` or a `drive.sh <scenario>`), 👁 (only
seen on a screen: the node shows `img: "shot:<id>.png"`), or gap (nothing covers it: the node is in
the flow's `gap` class). `check` fails when a node has no row, a row names no node, a label and its
node disagree, a pointer doesn't resolve, a shot isn't under `shots/<feature id>/`, a node can't be
reached from the start, or a node with no arrow out isn't in an end class (done, nothing, stopped).

Stdlib only, so a repo can vendor it as scripts/featuremap.py and run it in CI. A user flow is
parsed by Pele, the Mermaid parser, run with node and imported from beside this script.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_MAP = "docs/features"
LIST_KEYS = ("paths", "entry", "tests")


@dataclass
class Feature:
    id: str
    file: Path
    title: str = ""
    body: str = ""
    paths: list[str] = field(default_factory=list)
    entry: list[str] = field(default_factory=list)
    tests: list[str] = field(default_factory=list)

    def touches(self, path: str) -> bool:
        return any(glob_match(g, path) for g in self.paths)

    def section(self, heading: str) -> str:
        m = re.search(rf"^## {re.escape(heading)}\s*\n(.*?)(?=^## |\Z)", self.body, re.M | re.S)
        return m.group(1).strip() if m else ""


class MapError(Exception):
    pass


def _scalar(v: str) -> str:
    """A value with its trailing `# comment` dropped; a quoted value keeps any `#` inside it."""
    v = v.strip()
    if v[:1] == '"':  # YAML double quotes escape like JSON
        m = re.match(r'"(?:[^"\\]|\\.)*"', v)
        if m:
            try:
                return json.loads(m.group(0))
            except ValueError as e:
                raise MapError(f"can't read {v!r}: {e}") from e
    elif v[:1] == "'":  # YAML single quotes escape ' as ''
        m = re.match(r"'(?:[^']|'')*'", v)
        if m:
            return m.group(0)[1:-1].replace("''", "'")
    return v.split(" #", 1)[0].strip()


def parse(path: Path) -> Feature:
    """One feature file. Frontmatter supports `key: value` and `key:` + `- item` lists only."""
    text = path.read_text()
    m = re.match(r"---\n(.*?)\n---\n?(.*)", text, re.S)
    if not m:
        raise MapError(f"{path}: no frontmatter (--- ... ---)")
    meta: dict[str, object] = {}
    key = None
    for n, ln in enumerate(m.group(1).splitlines(), 2):
        if not ln.strip() or ln.lstrip().startswith("#"):
            continue
        item = re.match(r"\s+-\s+(.*)", ln)
        if item:
            if key not in LIST_KEYS:
                raise MapError(f"{path}:{n}: list item outside paths/entry/tests")
            meta.setdefault(key, []).append(_scalar(item.group(1)))  # type: ignore[union-attr]
            continue
        kv = re.match(r"([A-Za-z_][\w-]*):\s*(.*)", ln)
        if not kv:
            raise MapError(f"{path}:{n}: can't read {ln!r}")
        key = kv.group(1)
        val = "" if kv.group(2).startswith("#") else _scalar(kv.group(2))
        meta[key] = [] if key in LIST_KEYS else val
        if key in LIST_KEYS and val:
            raise MapError(f"{path}:{n}: {key} must be a list, one `- item` per line")
    if not meta.get("id"):
        raise MapError(f"{path}: frontmatter has no id")
    body = m.group(2)
    title = re.search(r"^# (.+)$", body, re.M)
    return Feature(id=str(meta["id"]), file=path, title=title.group(1).strip() if title else "",
                   body=body, **{k: list(meta.get(k) or []) for k in LIST_KEYS})  # type: ignore[arg-type]


def load(map_dir: Path) -> list[Feature]:
    if not map_dir.is_dir():
        raise MapError(f"no feature map at {map_dir}")
    return [parse(p) for p in sorted(map_dir.glob("*.md")) if p.name != "README.md"]


def glob_match(pattern: str, path: str) -> bool:
    """`**` spans directories, `*` and `?` stay within one path segment."""
    rx, i = "", 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            rx, i = rx + "(?:.*/)?", i + 3
        elif pattern.startswith("**", i):
            rx, i = rx + ".*", i + 2
        elif pattern[i] == "*":
            rx, i = rx + "[^/]*", i + 1
        elif pattern[i] == "?":
            rx, i = rx + "[^/]", i + 1
        else:
            rx, i = rx + re.escape(pattern[i]), i + 1
    return re.fullmatch(rx, path) is not None


def git(root: Path, *args: str) -> str:
    p = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    if p.returncode:
        raise MapError(f"git {' '.join(args)}: {p.stderr.strip()}")
    return p.stdout


def _pointer(root: Path, ref: str) -> tuple[Path, str]:
    file, _, name = ref.partition(":")
    return root / file, name.strip()


def check(root: Path, features: list[Feature]) -> tuple[list[str], list[str]]:
    """Problems that fail the check, and notes that don't."""
    tracked = [t for t in git(root, "ls-files", "-z").split("\0") if t]
    problems, notes, seen = [], [], {}
    for f in features:
        where = f.file.relative_to(root)
        if f.id in seen:
            problems.append(f"{where}: id {f.id!r} is also used by {seen[f.id]}")
        seen[f.id] = where
        if not f.paths:
            problems.append(f"{where}: no paths, so no change ever maps to {f.id}")
        for g in f.paths:
            if not any(glob_match(g, t) for t in tracked):
                problems.append(f"{where}: path {g!r} matches no tracked file")
        for kind in ("entry", "tests"):
            for ref in getattr(f, kind):
                file, name = _pointer(root, ref)
                if not file.is_file():
                    problems.append(f"{where}: {kind} {ref!r}: no file {file.relative_to(root)}")
                elif name and name not in file.read_text(errors="replace"):
                    problems.append(f"{where}: {kind} {ref!r}: {name!r} not found in the file")
        if not f.tests:
            notes.append(f"{where}: lists no tests")
        problems += [f"{where}: user flow: {p}" for p in check_flow(root, f)]
    return problems, notes


FLOW_LABELS = {"✓": "asserted", "👁": "observed", "gap": "gap"}


# Pele (obsidianmd/pele), the parser huddles-up draws flows with, reads the flowchart, so the check
# sees the diagram as it renders. It is imported from beside this script, never from the checked
# repo, so checking an untrusted branch runs none of its code.
PELE = """
const { detectType, load, parse } = await import("pele/lazy");
let source = "";
for await (const chunk of process.stdin) source += chunk;
const type = detectType(source);
if (type !== "flowchart") throw new Error(`a user flow is a flowchart, not ${type ?? "unknown"}`);
await load(type);
const model = parse(source);
process.stdout.write(JSON.stringify({
  nodes: [...model.nodes.values()].map((n) => ({
    id: n.id, type: n.type ?? null, text: n.text ?? n.id, img: n.img ?? null, classes: n.classes ?? [],
  })),
  edges: model.edges.map((e) => ({ start: e.start, end: e.end, type: e.type })),
}));
"""


@dataclass
class FlowNode:
    id: str
    type: str | None  # f-circ, round, diamond, hexagon, ..; None for one only an arrow names
    text: str
    img: str | None
    classes: set[str]


def flow_model(source: str) -> tuple[dict[str, FlowNode], list[tuple[str, str]]]:
    """The flowchart's nodes, in order of first mention, and its (from, to) edges, by Pele's parse.
    Raises MapError when Pele can't run or the flowchart doesn't parse."""
    try:
        run = subprocess.run(["node", "--input-type=module", "-e", PELE], cwd=Path(__file__).resolve().parent, input=source,
                             capture_output=True, text=True, timeout=60)
    except FileNotFoundError:
        raise MapError("a user flow is parsed by Pele, which needs node on PATH") from None
    if run.returncode:
        err = run.stderr.strip()
        if "ERR_MODULE_NOT_FOUND" in err and "pele" in err:
            raise MapError(f"a user flow is parsed by Pele: install `pele` where {Path(__file__).name} can import it")
        lines = [l for l in err.splitlines() if l.strip() and not l.lstrip().startswith("at ")]
        raise MapError("the flow doesn't parse: " + " ".join(lines[-4:]))
    model = json.loads(run.stdout)
    nodes = {n["id"]: FlowNode(n["id"], n["type"], n["text"], n["img"], set(n["classes"]))
             for n in model["nodes"]}
    edges: list[tuple[str, str]] = []
    for e in model["edges"]:
        edges.append((e["start"], e["end"]))
        if e["type"].startswith("double_"):  # `<-->` runs both ways
            edges.append((e["end"], e["start"]))
    return nodes, edges


FLOW_ENDS = ("done", "nothing", "stopped")


def check_drive(root: Path, scenario: str) -> str | None:
    """Why `drive.sh <scenario>` names no drive the repo can rerun, or None."""
    if not scenario:
        return "names no scenario"
    # Only committed files count: a local, ignored verify skill isn't there in CI or a fresh clone.
    tracked = set(git(root, "ls-files", "-z", "--", ".claude/skills/verify-*/*", ".agents/skills/verify-*/*").split("\0"))
    drives = sorted(root / t for t in tracked if t.endswith("/drive.sh") and (root / t).is_file())
    if not drives:
        return "no committed verify-*/drive.sh in the repo"
    # A verify skill names its scenarios on a `Scenarios:` line, in backticks, or in a documented
    # `drive.sh <scenario>` command; prose elsewhere names none.
    known: set[str] = set()
    for d in drives:
        doc = d.parent / "SKILL.md"
        text = doc.read_text(errors="replace") if doc.relative_to(root).as_posix() in tracked and doc.is_file() else ""
        for line in re.findall(r"^\s*Scenarios:(.*)$", text, re.M):
            known.update(w for quoted in re.findall(r"`([^`]+)`", line) for w in quoted.split())
        # The whole token is the scenario (`send.v2`), less a sentence's closing punctuation.
        known.update(t.rstrip(".,;:") for t in re.findall(r"drive\.sh\s+([^\s`'\"|()]+)", text))
    name = scenario.split()[0]
    return None if name in known else f"no scenario {name!r} in the verify skill"


def check_flow(root: Path, f: Feature) -> list[str]:
    """What a feature's `## User flow` gets wrong: every node a proof row, every row a node."""
    flow = f.section("User flow")
    if not re.search(r"^## User flow\s*$", f.body, re.M):
        return []
    fence = re.search(r"```mermaid\n(.*?)\n```", flow, re.S)
    if not fence:
        return ["no ```mermaid fence"]
    try:
        nodes, edges = flow_model(fence.group(1))
    except MapError as e:
        return [str(e)]
    if not nodes:
        return ["the flow has no states"]
    gaps = {node for node, n in nodes.items() if "gap" in n.classes}
    rows: dict[str, tuple[str, str]] = {}
    problems = []
    for cells in re.findall(r"^\|\s*`([^`\s|]+)`\s*\|\s*([^|]+?)\s*\|\s*(.+?)\s*\|\s*$", flow, re.M):
        node, label, proof = cells
        if node in rows:
            problems.append(f"`{node}` has two proof rows")
        rows[node] = (label, proof)
    # The start is where the member is, and a decision (a `{diamond}`, not a `{{hexagon}}`) is a
    # question; every other node is a claim.
    # A start drawn as a statechart's filled dot is no screen, so the state it enters is the start.
    dots = [node for node, n in nodes.items() if n.type == "f-circ"]
    start = dots[0] if dots else next(iter(nodes), None)
    where = {start} | {b for a, b in edges if a == start and dots}
    for node, n in nodes.items():
        if node not in where and node not in rows and n.type != "diamond":
            problems.append(f"`{node}` has no proof row")
    # A statechart's rigor: every state is reachable from the start, and one with no way out is an
    # end that says how it ended.
    reached, todo = set(), [start]
    while todo:
        node = todo.pop()
        if node is not None and node not in reached:
            reached.add(node)
            todo += [b for a, b in edges if a == node]
    leaving = {a for a, _ in edges}
    shots = (root / f.file.parent / "shots" / f.id).resolve()
    for node, n in nodes.items():
        # Every node's shot resolves inside its feature's shots/, proof row or not.
        shot = n.img[len("shot:"):] if n.img and n.img.startswith("shot:") else None
        if shot is not None and not (shots / shot).resolve().is_relative_to(shots):
            problems.append(f"`{node}`: shot {shot!r} is outside shots/{f.id}/")
        elif shot is not None and not (shots / shot).is_file():
            problems.append(f"`{node}`: shot {shot!r} is not in shots/{f.id}/")
        if node not in reached:
            problems.append(f"`{node}` can't be reached from the start")
        elif node not in leaving and not n.classes & set(FLOW_ENDS):
            problems.append(f"`{node}` has no way out, so it is an end: put it in class {', '.join(FLOW_ENDS)}")
    for node, (label, proof) in rows.items():
        n = nodes.get(node)
        if n is None:
            problems.append(f"row `{node}` names no node in the flow")
            continue
        if label not in FLOW_LABELS:
            problems.append(f"`{node}`: label {label!r} is not ✓, 👁 or gap")
            continue
        if (label == "gap") != (node in gaps):
            problems.append(f"`{node}`: labelled {label} but {'not ' if node not in gaps else ''}in the gap class")
        if label != "gap" and not n.text.strip().startswith(label):
            problems.append(f"`{node}`: labelled {label} but its text doesn't start with it")
        if label == "👁" and not (n.img or "").startswith("shot:"):
            problems.append(f"`{node}`: observed, but shows no shot")
        # A gap names what is missing; its code spans are prose, not pointers.
        refs = [] if label == "gap" else re.findall(r"`([^`]+)`", proof)
        drives = [r[len("drive.sh "):].strip() for r in refs if r.startswith("drive.sh ")]
        tests = [r for r in refs if not r.startswith("drive.sh ")]
        # ✓ needs a named test (file:test) or a drive; a bare file asserts nothing.
        if label == "✓" and not drives and not any(_pointer(root, r)[1] for r in tests):
            problems.append(f"`{node}`: asserted, but its proof names no test or drive")
        for scenario in drives:
            problem = check_drive(root, scenario)
            if problem:
                problems.append(f"`{node}`: proof `drive.sh {scenario}`: {problem}")
        for ref in tests:
            file, name = _pointer(root, ref)
            if not file.is_file():
                problems.append(f"`{node}`: proof {ref!r}: no file {file.relative_to(root) if file.is_relative_to(root) else file}")
            elif name and name not in file.read_text(errors="replace"):
                problems.append(f"`{node}`: proof {ref!r}: {name!r} not found in the file")
    return problems


# A vendored copy's second line is `# vendored from <repo>@<sha>; sha256 <hex>`, the hash of
# `<repo>@<sha>` and a newline, then the copy without that line (the canonical file at <sha>), so
# the provenance can't change either. Changes land in <repo>, never in a copy.
VENDORED = re.compile(rb"^# vendored from (\S+)@([0-9a-f]{7,40}); sha256 ([0-9a-f]{64})\n", re.M)


def vendored_hash(src: bytes, sha: bytes, canonical: bytes) -> str:
    return hashlib.sha256(src + b"@" + sha + b"\n" + canonical).hexdigest()


def vendored_drift(script: Path) -> str | None:
    """Why a vendored copy no longer matches the file it was vendored from, or None."""
    text = script.read_bytes()
    m = VENDORED.search(text)
    # A pin that no longer parses (CRLF, a mangled hash) is drift too, not an unpinned file.
    if not m and re.search(rb"^# vendored from ", text, re.M):
        return f"{script.name} has a vendored-from line that doesn't parse: vendor it again"
    if not m or vendored_hash(m.group(1), m.group(2), text[:m.start()] + text[m.end():]) == m.group(3).decode():
        return None
    src, sha = m.group(1).decode(), m.group(2).decode()[:12]
    return (f"{script.name} was edited after it was vendored from {src}@{sha}: make the change in "
            f"{src} and vendor it again")


def changed_files(root: Path, base: str) -> list[str]:
    # -z: unquoted names. --no-renames: a file moved out of a feature still touches it.
    out = git(root, "diff", "--name-only", "--no-renames", "-z", f"{base}...HEAD")
    return [p for p in out.split("\0") if p]


def stale(root: Path, f: Feature) -> tuple[str, list[str]] | None:
    """The commit that last changed the feature's file, and its paths changed since then."""
    # The map was last true when its file last changed: a commit can't name its own sha, and a
    # squash merge drops the branch's shas, so a recorded `verified:` commit couldn't hold.
    rel = f.file.relative_to(root).as_posix()
    if git(root, "status", "--porcelain", "--", rel).strip():
        return None  # being edited now
    last = git(root, "log", "-1", "--format=%h %cs", "--", rel).strip()
    if not last:
        return None
    # Against the working tree, so staged and unstaged code edits count too.
    out = git(root, "diff", "--name-only", "--no-renames", "-z", last.split()[0])
    changed = [p for p in out.split("\0") if p and f.touches(p)]
    return (last, changed) if changed else None


def touched(features: list[Feature], files: list[str]) -> tuple[list[Feature], list[str]]:
    hit = [f for f in features if any(f.touches(p) for p in files)]
    unmapped = [p for p in files if not any(f.touches(p) for f in features)]
    return hit, unmapped


def render(root: Path, hit: list[Feature], unmapped: list[str]) -> str:
    out = []
    for f in hit:
        out.append(f"## {f.id}: {f.title}  ({f.file.relative_to(root)})")
        if f.entry:
            out.append("Start reading: " + ", ".join(f"`{e}`" for e in f.entry))
        if f.tests:
            out.append("Tests: " + ", ".join(f"`{t}`" for t in f.tests))
        drift = stale(root, f)
        if drift:
            (last, changed), more = drift, len(drift[1]) - 10
            out.append(f"Possibly stale: {len(changed)} file(s) under its paths changed since its "
                       f"file last did ({last}): " + ", ".join(changed[:10]) +
                       (f" and {more} more" if more > 0 else ""))
        gotchas = f.section("Gotchas")
        if gotchas:
            out.append("Gotchas:\n" + gotchas)
        out.append("")
    if not hit:
        out.append("No mapped feature is touched.")
    if unmapped:
        out.append(f"Unmapped ({len(unmapped)}): " + ", ".join(unmapped[:20]) +
                   (" …" if len(unmapped) > 20 else ""))
    return "\n".join(out).rstrip() + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--root", default=".", help="repository root (default: .)")
    ap.add_argument("--map", default=DEFAULT_MAP, help=f"map directory (default: {DEFAULT_MAP})")
    sub = ap.add_subparsers(dest="cmd", required=True)
    fe = sub.add_parser("features", help="the features a change touches")
    fe.add_argument("--base", help="diff BASE...HEAD instead of naming files")
    fe.add_argument("files", nargs="*")
    sub.add_parser("check", help="exit 1 when a pointer no longer resolves")
    a = ap.parse_args(argv)
    try:
        root = Path(git(Path(a.root), "rev-parse", "--show-toplevel").strip())
        features = load(root / a.map)
        if a.cmd == "check":
            problems, notes = check(root, features)
            drift = vendored_drift(Path(__file__))
            if drift:
                problems.append(drift)
            for n in notes:
                print(f"note: {n}")
            for p in problems:
                print(f"error: {p}")
            print(f"{len(features)} features, {len(problems)} broken pointer(s)")
            return 1 if problems else 0
        if not a.files and not a.base:
            ap.error("features needs FILE ... or --base REF")
        files = a.files or changed_files(root, a.base)
        print(render(root, *touched(features, files)), end="")
        return 0
    except MapError as e:
        print(f"featuremap: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
