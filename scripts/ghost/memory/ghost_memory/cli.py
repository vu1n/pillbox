#!/usr/bin/env python3
"""ghost-memory — one entrypoint over the engine's commands. `serve` (the MCP) is the primary; the
rest are the capture/maintenance CLIs. Each subcommand delegates to its module's main(); subcommand
mains/imports stay lazy so e.g. `serve` doesn't import the capture stack (or vice-versa)."""
import sys

_COMMANDS = {
    "serve": "the MCP server (observe/claim/recall/decide/remember_procedure/consolidate/resolve_conflicts)",
    "capture": "capture one session's §0 log into memory (alias: wire)",
    "sweep": "autocapture: sweep completed sessions into memory (idempotent)",
    "consolidate": "dedup near-duplicate claims (exact + optional semantic)",
    "batch": "LLM re-distill a corpus of §0 logs, one representative per task",
}
_USAGE = "usage: ghost-memory {" + "|".join(_COMMANDS) + "} [args...]\n\n" + \
    "\n".join(f"  {c:<12} {d}" for c, d in _COMMANDS.items())


def main():
    if len(sys.argv) < 2 or sys.argv[1] in ("-h", "--help"):
        print(_USAGE)
        return
    cmd = sys.argv[1]
    # Hand the subcommand's own main() a clean argv (its prog name + its args) so its argparse works.
    sys.argv = [f"ghost-memory {cmd}", *sys.argv[2:]]
    if cmd == "serve":
        from .mcp_server import main as run
    elif cmd in ("capture", "wire"):
        from .wire import main as run
    elif cmd == "sweep":
        from .autocapture import main as run
    elif cmd == "consolidate":
        from .arbiter import main as run
    elif cmd == "batch":
        from .batch_distill import main as run
    else:
        print(f"ghost-memory: unknown command {cmd!r}\n{_USAGE}", file=sys.stderr)
        sys.exit(2)
    run()


if __name__ == "__main__":
    main()
