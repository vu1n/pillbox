#!/usr/bin/env python3
"""Hash only this smoke project's invocation/evidence files, never auth paths."""
import argparse
import json
from pathlib import Path
from inspect_completion import bounded_read
from prepare import digest


def fingerprint(state_dir):
    result = {}
    for name in ("repository-executions", "sessions"):
        directory = state_dir / name
        assert not directory.is_symlink()
        for path in sorted(directory.rglob("*")):
            assert not path.is_symlink()
            if path.is_file():
                result[str(path.relative_to(state_dir))] = digest(bounded_read(path))
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, required=True)
    args = parser.parse_args()
    assert __debug__, "do not disable smoke assertions"
    print(json.dumps(fingerprint(args.state_dir), sort_keys=True, indent=2))
