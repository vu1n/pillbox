# code-explore — read-only repo-orientation MCP sidecar (#69)

A host-side HTTP MCP server that gives a sandboxed agent **read-only code
exploration** — "where is X / what implements Y" answered as compact `path:line`
citations, so the agent spends its context budget editing and testing, not
grepping. Attached to a run with pillbox's existing `--mcp` wiring (no pillbox
code involved — see [`docs/shared-mcp.md`](../../../docs/shared-mcp.md)).

Backend is **deterministic `rg` + `ast-grep`** — zero serving, zero GPU,
reproducible. It is *not* a model.

## Tools

| tool | backend | what |
|---|---|---|
| `explore_code(query, max_results=20, path="")` | ripgrep | NL orientation: searches the query's identifier terms, ranks files by how many distinct terms they cover, returns the top `path:line` citations. |
| `find_pattern(pattern, lang, max_results=20, path="")` | ast-grep | structural search — match a code *shape* (`$VAR` one node, `$$$ARGS` many), e.g. `foo($$$ARGS)` in `lang="ts"`. |

Both are read-only (they shell `rg`/`ast-grep`, never write). `path` is confined
to the exploration root (a traversal-guarded subdirectory restriction).

## Setup

Needs **Python 3.10+** (the `mcp` SDK floor) plus `rg` and `ast-grep` on PATH.
Simplest with [`uv`](https://docs.astral.sh/uv/) (picks the interpreter for you):

```sh
cd scripts/mcp/code-explore
uv run --python 3.12 --with 'mcp>=1.12' server.py --self-test   # offline gate
```

Or a classic venv (only if your default `python3` is ≥ 3.10):

```sh
python3 -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt
python server.py --self-test
```

The `--self-test` runs the pure search logic + one live `rg` query against this
repo — no `mcp` install needed (the search logic is decoupled from the transport),
so it's the fast gate before serving.

## Run + attach

The sidecar must point at the **repo the agent is working on**, host-side
(`EXPLORE_ROOT`). For a `pillbox run` over a mounted cwd, that's the project dir;
for a forked/cloned workspace it's the host-visible clone path (libkrun exposes it
via `pillbox session info <id> --json` → `.session.workspace`, the same path
grading reads). It reads orientation state (the committed code structure), not the
agent's latest uncommitted edits — exactly what "where is X" needs.

```sh
# 1. serve it, rooted at the repo (uv picks the interpreter + deps)
EXPLORE_ROOT="$PWD" uv run --python 3.12 --with 'mcp>=1.12' \
  scripts/mcp/code-explore/server.py --port 8123

# 2. attach it to a run (pillbox rewrites localhost → host.docker.internal)
pillbox run --mcp code-explore=http://localhost:8123 -- "…task…"
```

The agent then sees `explore_code` / `find_pattern` in its MCP tool list.

## Swapping in FastContext later (large codebases)

The contract — an MCP tool named **`explore_code`** taking a **`query`** and
returning a **`<final_answer>` citation block** — is deliberately the same shape
[Microsoft FastContext](https://github.com/microsoft/fastcontext) exposes
(`fastcontext --query … --max-turns N`). FastContext is a 4B agentic explorer
(multi-turn Read/Glob/Grep, ranks relevance) that beats deterministic grep on
*large* codebases — but it needs a self-served OpenAI-compatible endpoint for its
model (`FastContext-1.0-4B-SFT`, not hosted by any provider; SGLang with
`--tool-call-parser qwen` is the reliable serve).

Because the MCP contract is identical, a FastContext-backed sidecar drops in
behind the **same `--mcp code-explore=URL`** with no change to pillbox or the
agent. This rg backend is the cheap, always-available default; FastContext is the
scale upgrade. (You can even point a FastContext wrapper's `BASE_URL` at an
existing API endpoint to validate it before standing up the 4B serve.)

## Limits

- Reads host-side, so it sees committed/mounted state, not in-sandbox uncommitted
  edits — fine for orientation, not for "what did I just change."
- `explore_code` ranking is term-coverage, not semantic; for a *shape* query use
  `find_pattern`. For genuinely large repos where an agentic multi-turn explorer
  wins, that's the FastContext swap above.
- Binds `127.0.0.1` by default (reachable from Docker Desktop via
  `host.docker.internal`). Set `--host 0.0.0.0` if your container networking needs
  it; the server has no auth (pillbox's `--mcp-token` adds a bearer if you want one).
