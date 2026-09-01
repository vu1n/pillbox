#!/usr/bin/env python3
"""Generate a per-test-method rubric.txt for each aider-polyglot task.

A binary `unittest discover` grade is all-or-nothing; a rubric scores the
*fraction* of test methods that pass — a real gradient (and per-criterion
feedback naming which tests failed), which is what `session score --rubric`
and the GEPA arm consume. One criterion per `test_*` method:

    <Class.method> :: python3 -m unittest <test_module>.<Class>.<method>

The command runs with cwd = the agent's edited workspace (where run-task.sh
injects the hidden test module at grade time), so it never reaches the agent.

Usage: gen-rubrics.py [tasks-dir]   (default scripts/eval/tasks)
"""
import ast
import os
import sys


def methods(test_path: str):
    """(class, method) for every test_* method in a *_test.py, via AST."""
    tree = ast.parse(open(test_path).read())
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef):
            for item in node.body:
                if isinstance(item, ast.FunctionDef) and item.name.startswith("test"):
                    yield node.name, item.name


def main() -> None:
    here = os.path.dirname(os.path.abspath(__file__))
    tasks = sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, "tasks")
    n = 0
    for task in sorted(os.listdir(tasks)):
        grader = os.path.join(tasks, task, "grader")
        if not os.path.isdir(grader):
            continue
        tests = [f for f in os.listdir(grader) if f.endswith("_test.py")]
        if len(tests) != 1:
            print(f"skip {task}: expected 1 *_test.py, found {len(tests)}", file=sys.stderr)
            continue
        mod = tests[0][:-3]  # drop .py → the importable test module name
        crits = [
            f"{cls}.{m} :: python3 -m unittest {mod}.{cls}.{m}"
            for cls, m in methods(os.path.join(grader, tests[0]))
        ]
        if not crits:
            print(f"skip {task}: no test methods", file=sys.stderr)
            continue
        out = os.path.join(grader, "rubric.txt")
        with open(out, "w") as f:
            f.write(
                "# Per-test-method rubric (auto-generated). Each criterion runs one\n"
                "# unittest method against the agent's solution + the hidden test\n"
                "# module (injected at grade time). Score = fraction passing.\n"
            )
            f.write("\n".join(crits) + "\n")
        print(f"{task}: {len(crits)} criteria → {out}")
        n += 1
    print(f"\nwrote {n} rubrics")


if __name__ == "__main__":
    main()
