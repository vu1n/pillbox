#!/usr/bin/env python3
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

    features [--base REF] [FILE ...]   the features a change touches, with their tests and Gotchas
    check                              exit 1 when a pointer no longer resolves

Stdlib only, so a repo can vendor it as scripts/featuremap.py and run it in CI.
"""
from __future__ import annotations

import argparse
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
    return problems, notes


def changed_files(root: Path, base: str) -> list[str]:
    # -z: unquoted names. --no-renames: a file moved out of a feature still touches it.
    out = git(root, "diff", "--name-only", "--no-renames", "-z", f"{base}...HEAD")
    return [p for p in out.split("\0") if p]


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
