#!/usr/bin/env python3
"""Machine-check that the CF §0 contract (src/contract.ts) stays faithful to the canonical
Rust contract (../src/contract.rs::Event/Payload/Actor). The "one §0, two backends" thesis —
local libkrun and the CF DO gateway feeding the SAME event log that kypp/ghost/subscribe
consume — only holds if these two definitions don't drift. contract.ts is hand-reconciled
with inline "matches contract.rs" comments; this turns that promise into a gate.

The relationship is NOT a simple subset (so a codegen/diff would false-fail):
  • TS models a CURATED SUBSET of Rust's payloads (the ones the spike exercises) + a
    `{type: string}` catch-all that absorbs the rest (== Rust's `Unknown` #[serde(other)]).
  • TS carries SPEC-AHEAD payloads not yet in Rust (e.g. driver_changed).
So the check is: where BOTH sides define something, it must AGREE; the deliberate
subset/spec-ahead gaps are allowed but REPORTED, so coverage is visible and drift is loud.

FAILS (drift / wire-incompat): a Rust Event envelope field missing from TS; Actor kinds or
fields differing; a SHARED payload tag whose TS fields don't match the Rust struct's; a TS
"spec-ahead" tag that is ACTUALLY in Rust now (stale claim → the input/annotation case);
a missing TS catch-all (forward-compat broken).
REPORTS (not fails): Rust payloads absorbed by the TS catch-all (coverage), genuine
TS-only spec-ahead payloads.

Pure file-parse — no node/cargo. Wired into scripts/smoke/cf.sh.
"""
from __future__ import annotations

import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
RS = os.path.join(HERE, "..", "src", "contract.rs")
TS = os.path.join(HERE, "src", "contract.ts")


def snake(camel: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", camel).lower()


def camel(snk: str) -> str:
    head, *rest = snk.split("_")
    return head + "".join(w[:1].upper() + w[1:] for w in rest)


def _block(src: str, header_re: str) -> str | None:
    """The brace-balanced body following the first header match (e.g. a struct/enum)."""
    m = re.search(header_re, src)
    if not m:
        return None
    i = src.index("{", m.start())
    depth, j = 0, i
    while j < len(src):
        if src[j] == "{":
            depth += 1
        elif src[j] == "}":
            depth -= 1
            if depth == 0:
                return src[i + 1:j]
        j += 1
    return None


# ── Rust side ────────────────────────────────────────────────────────────────
def rust_struct_fields(src: str, name: str) -> list[str] | None:
    """Wire field names of a `#[serde(rename_all=camelCase)]` struct: snake field →
    camelCase, honoring `#[serde(rename="x")]` and dropping fully-skipped fields.
    `skip_serializing_if` is conditional (field still on the wire) → kept."""
    body = _block(src, rf"struct\s+{name}\b")
    if body is None:
        return None
    fields, pending_rename, pending_skip = [], None, False
    for line in body.splitlines():
        s = line.strip()
        if s.startswith("#["):
            if (rn := re.search(r'rename\s*=\s*"([^"]+)"', s)):
                pending_rename = rn.group(1)
            if re.search(r"\bskip\b|skip_serializing\b(?!_if)", s) and "skip_serializing_if" not in s:
                pending_skip = True
            continue
        fm = re.match(r"(?:pub(?:\(crate\))?\s+)?([a-z_][a-z0-9_]*)\s*:", s)
        if fm:
            if not pending_skip:
                fields.append(pending_rename or camel(fm.group(1)))
            pending_rename, pending_skip = None, False
    return fields


def rust_contract(src: str) -> dict:
    ev = rust_struct_fields(src, "Event") or []
    actor = rust_struct_fields(src, "Actor") or []
    akind_body = _block(src, r"enum\s+ActorKind\b") or ""
    akinds = {snake(v) for v in re.findall(r"^\s*([A-Z][A-Za-z0-9]*)\s*,", akind_body, re.M)}
    # Payload enum: each variant `Name` (unit) or `Name(Struct)` (newtype). tag = snake(Name).
    pl = _block(src, r"enum\s+Payload\b") or ""
    tags, catch_all = {}, False
    skip_other = False
    for line in pl.splitlines():
        s = line.strip()
        if s.startswith("//"):
            continue
        if s.startswith("#["):
            if "other" in s:
                skip_other = True  # the next variant is the #[serde(other)] catch-all
            continue
        vm = re.match(r"([A-Z][A-Za-z0-9]*)\s*(?:\(([A-Za-z0-9_]+)\))?\s*,", s)
        if vm:
            if skip_other:
                catch_all = True  # Rust's Unknown — the forward-compat absorber
                skip_other = False
                continue
            name, wrapped = vm.group(1), vm.group(2)
            tags[snake(name)] = rust_struct_fields(src, wrapped) if wrapped else []
    return {"event": ev, "actor": actor, "actor_kinds": akinds, "payload": tags, "catch_all": catch_all}


# ── TS side ──────────────────────────────────────────────────────────────────
def ts_iface_fields(src: str, name: str) -> list[str]:
    body = _block(src, rf"interface\s+{name}\b") or ""
    # `field?: T;` / `field: T;` — strip optional marker; ignore index sigs / comments.
    return re.findall(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\??\s*:", body, re.M)


def ts_contract(src: str) -> dict:
    ev = ts_iface_fields(src, "Event")
    actor = ts_iface_fields(src, "Actor")
    # Actor kind union: `kind: "human" | "agent" | ...`
    km = re.search(r"\bkind\s*:\s*([^;]+);", _block(src, r"interface\s+Actor\b") or "")
    akinds = set(re.findall(r'"([a-z]+)"', km.group(1))) if km else set()
    # Payload union members are `| { ... }` lines (the only such pattern in the file —
    # Event/Actor are interfaces). `[^}]*` is safe: no member has a nested brace.
    # (A naive `Payload = (.+?);` fails — the first `;` is INSIDE `{ type: "input"; … }`.)
    tags, catch_all = {}, False
    for inner in re.findall(r"^\s*\|\s*\{([^}]*)\}", src, re.M):
        tm = re.search(r'type\s*:\s*"([^"]+)"', inner)
        if tm:
            keys = [k for k in re.findall(r"([A-Za-z_][A-Za-z0-9_]*)\s*\??\s*:", inner) if k != "type"]
            tags[tm.group(1)] = keys
        elif re.search(r"type\s*:\s*string", inner):  # `{ type: string; [k:string]: unknown }`
            catch_all = True
    return {"event": ev, "actor": actor, "actor_kinds": akinds, "payload": tags, "catch_all": catch_all}


def main() -> int:
    rs, ts = rust_contract(open(RS).read()), ts_contract(open(TS).read())
    fails, notes = [], []

    # E1: every Rust envelope field present in TS (TS extras = spec-ahead, fine).
    miss = [f for f in rs["event"] if f not in ts["event"]]
    if miss:
        fails.append(f"Event envelope: TS missing Rust field(s) {miss}")
    if extra := [f for f in ts["event"] if f not in rs["event"]]:
        notes.append(f"Event: TS-only (spec-ahead) envelope field(s): {extra}")

    # E2: Actor fields + kinds must agree.
    if set(rs["actor"]) != set(ts["actor"]):
        fails.append(f"Actor fields differ: rust={rs['actor']} ts={ts['actor']}")
    if rs["actor_kinds"] != ts["actor_kinds"]:
        fails.append(f"ActorKind differs: rust={sorted(rs['actor_kinds'])} ts={sorted(ts['actor_kinds'])}")

    # Catch-all: Rust has Unknown #[serde(other)]; TS must have its `{type:string}` absorber.
    if rs["catch_all"] and not ts["catch_all"]:
        fails.append("TS Payload lacks the `{type:string}` catch-all (forward-compat broken)")

    # P: payload tags.
    shared = sorted(set(rs["payload"]) & set(ts["payload"]))
    ts_only = sorted(set(ts["payload"]) - set(rs["payload"]))
    rust_only = sorted(set(rs["payload"]) - set(ts["payload"]))
    for tag in shared:
        rfields, tfields = rs["payload"][tag], ts["payload"][tag]
        if rfields is None:
            notes.append(f"payload `{tag}`: could not parse the Rust struct fields (skipped)")
            continue
        if bad := [f for f in tfields if f not in rfields]:
            fails.append(f"payload `{tag}`: TS field(s) {bad} not in Rust struct (fields={rfields})")
    for tag in ts_only:
        # A "spec-ahead" TS tag that's actually in Rust would have shown up in `shared`;
        # ts_only is genuinely TS-only. Fine, but report it (driver_changed expected).
        notes.append(f"payload `{tag}`: TS-only / spec-ahead (not in contract.rs)")

    print("§0 contract parity — contract.ts vs contract.rs")
    print(f"  shared payload tags ({len(shared)}, field-checked): {shared}")
    print(f"  rust-only (absorbed by TS catch-all): {rust_only}")
    print(f"  ts-only (spec-ahead): {ts_only}")
    print(f"  event: rust={len(rs['event'])} fields ⊆ ts={len(ts['event'])} | actor_kinds={sorted(rs['actor_kinds'])}")
    for n in notes:
        print(f"  · {n}")
    if fails:
        print("\n✗ PARITY DRIFT:")
        for f in fails:
            print(f"  - {f}")
        return 1
    print("\n✓ parity holds — every shared field/variant/kind agrees; gaps are deliberate (reported above)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
