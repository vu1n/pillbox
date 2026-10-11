# Native refusal fixture provenance

`opencode-2.0.24-denied-tool.jsonl` derives from the OpenCode 2.0.24
local fake-provider capture in [PR184](https://github.com/vu1n/pillbox/pull/184),
at commit `4de34e09ce6be9b3133836e5440d10fe2489665a`, source
[`src/execution/fixtures/opencode-2.0.24-tool-attempt.jsonl`](https://github.com/vu1n/pillbox/blob/4de34e09ce6be9b3133836e5440d10fe2489665a/src/execution/fixtures/opencode-2.0.24-tool-attempt.jsonl).
The original file's SHA-256 is
`793872ed93d82eec46b45822bd59c2eab34f646981a8d75e487ea87891de8068`.

The derivative keeps the four tool events in their original order, drops
unrelated events and event IDs/timestamps/location/durable metadata, normalizes
session/message/call IDs to test identifiers, and replaces `/tmp/pwned` with
`/tmp/pillbox-fixture-sentinel` in both argument representations. The Rust test
only replays JSON; it never executes these arguments.

This capture proves the native event shape, not this adapter's sealed profile.
`bash` is an unavailable alias in OpenCode 2.0.24. The executable
`scripts/smoke/opencode-text/test_native_refusal.py` separately tests the exact
production configuration with only the provider transport redirected to a
verified loopback endpoint and checks the CLI version against the runner pin.
It forces `shell`, `read`, `write`, `edit`, `execute`,
`webfetch`, an MCP name and the `bash` alias, requires the native unavailable-tool
failure, verifies no tools were offered, and checks filesystem/network sentinels.
It uses no credentials or model service. `session.tool.called.executed:false`
alone is not proof of refusal; native local tools report that before execution.
