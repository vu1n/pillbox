# moved → `kypp`

The swarm-memory engine that lived here was spun out into its own project:

**https://github.com/vu1n/kypp**

It's independent of pillbox. Pillbox integrates it by **attaching** (point the agent's MCP at a
running `kypp serve --http` via `pillbox run --mcp kypp=<url>`) and **notifying** (on
`session.completed`, run `kypp sweep` to capture the §0 log) — pillbox does not own or spawn it.
