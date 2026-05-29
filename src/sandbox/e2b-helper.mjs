#!/usr/bin/env node
//
// pillbox-e2b-helper — Node subprocess that bridges local pillbox to an
// E2B sandbox running the attach-transport frame protocol.
//
// ## What this is (and what it is NOT)
//
// This helper is a **dumb byte shuttle**. It owns the E2B sandbox
// lifecycle (create / connect / kill) and nothing else. It does NOT
// parse the frame protocol, interpret Ctrl-A, manage raw mode, or track
// terminal size — all of that lives in the Rust `attach::pump`
// (`attach_terminal`), which runs on the host over THIS helper's
// stdin/stdout. The helper just carries bytes between its own stdio and
// the in-sandbox `pillbox pty-relay`, exactly the way the docker and ssh
// backends carry the identical protocol over `docker exec` / `ssh`
// stdio. See `docs/attach-transport.md`.
//
//   host terminal ─ Rust attach::pump ─ helper stdio ─┐
//                                                      │ (raw E2B pty)
//   agent ─ pty-host (unix sock) ─ pty-relay ──────────┘
//
//   - `pillbox pty-host` runs via `commands.run({background})`. It owns
//     the agent's real PTY (via portable-pty) and serves the frame
//     protocol on a unix socket. Its own stdout/stderr are just logs.
//   - `pillbox pty-relay` runs under a raw E2B `pty.create`. It bridges
//     the pty-host socket to its stdio; the raw pty keeps the binary
//     frames intact (E2B's `commands.run` string-decodes and would
//     corrupt them — that's why the relay needs a pty).
//
// Wire (set by the Rust caller in src/sandbox/remote_e2b.rs):
//   - argv (one of):
//       attach   --template TEMPLATE_ID --blob-file PATH --session-id ID
//                [--name N] [--detach] [--events-webhook URL] [--parent ID]
//       reattach --sandbox-id ID --session-id ID
//       kill     --sandbox-id ID
//   - env:   E2B_API_KEY=...   (read by the `e2b` SDK)
//   - stdin: raw attach-transport frames from the host pump → forwarded
//            verbatim to the relay's PTY. NO Ctrl-A interpretation here.
//   - stdout: raw attach-transport frames from the relay → host pump.
//   - stderr: helper diagnostics + the JSON handshake (below).
//
// ## Wire (stderr handshake)
//
// One JSON line per state transition, parsed by `pump_helper_stderr`
// in `src/sandbox/remote_e2b.rs`. Always sent before any free-text
// diagnostics so the Rust side can distinguish protocol from noise.
//
//   {type:"sandbox-up", protoVersion, sandboxId, pid?}
//       Sent once the pty-host socket is listening (attach/reattach) or
//       the sandbox is connected (kill). The Rust side echoes the id and
//       persists the session record (for `attach --detach` only). `pid`
//       is vestigial — reattach derives the pty-host socket path from the
//       session id, so the record's `pty_pid` is no longer used.
//   {type:"detached"}
//       Sent when `attach --detach` finishes launching the pty-host and
//       is about to exit without killing the sandbox.
//
// Detach (Ctrl-A D) is NO LONGER signalled by this helper: the host pump
// detects it and tears the helper down via SIGTERM. See the teardown
// matrix in `wireRelay` below.
//
// ## Why a temp file (not stdin) for the blob
//
// stdin is the frame channel, so it can't also carry the vault blob.
// The host stages the blob to a local temp file and we upload it into
// the sandbox via the Files API; the pty-host wrapper reads it with
// `pillbox run --vault-stdin < FILE` and unlinks it.
//
// ## Why a Node helper and not a Rust HTTP client
//
// No official E2B Rust SDK; the only third-party crate covers only
// code-interpreter (no PTY, no commands.run). Porting the SDK protocol
// natively is ~1.5K LOC of HTTP/WebSocket plumbing. The JS SDK is the
// supported surface; we embed a small subprocess.

import { Sandbox } from "e2b";
import { readFile } from "node:fs/promises";
import { randomBytes } from "node:crypto";

const PROTO_VERSION = 1;
const SANDBOX_TIMEOUT_MS = 3_600_000;

// OTLP env vars forwarded off our inherited host env into the sandbox
// wrapper so the sandbox-side tailer streams spans to the operator's
// collector. Keep in sync with FORWARDED_OTEL_VARS in remote_ssh.rs.
const OTEL_FORWARD_VARS = [
	"OTEL_EXPORTER_OTLP_ENDPOINT",
	"OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
	"OTEL_EXPORTER_OTLP_HEADERS",
	"OTEL_EXPORTER_OTLP_TIMEOUT",
	"OTEL_SERVICE_NAME",
];

// Control-byte marker printed by the relay launch line once the PTY is in
// raw mode, just before `exec pillbox pty-relay`. The helper streams
// everything AFTER this marker verbatim to stdout; everything before is
// the shell's cooked echo of the launch line and must be dropped (it
// would otherwise corrupt the host pump's frame decoder). RS (0x1e) bytes
// don't appear in that echo — the echo shows the literal text `\036`, not
// the byte — so there's no false match.
const RELAY_MARKER = Buffer.from([0x1e, 0x1e, 0x50, 0x42, 0x1e, 0x1e]); // \x1e\x1ePB\x1e\x1e

// Upper bound on each network teardown call (`pty.kill` / `sandbox.kill`).
// The host blocks in `child.wait()` until we exit, so a hung E2B API call
// during teardown must not wedge pillbox forever — we exit regardless once
// this elapses.
const TEARDOWN_TIMEOUT_MS = 5_000;

function fail(msg) {
	process.stderr.write(`pillbox-e2b-helper: ${msg}\n`);
	process.exit(1);
}

function notifyRust(payload) {
	process.stderr.write(`${JSON.stringify(payload)}\n`);
}

// Absorb EPIPE on our pipes to Rust. Once the host pump reads the agent's
// `Exit` frame it returns + drops its read end, but pty-host may emit one
// more `Data`/diagnostic before its child fully tears down; that lands in
// our `onData`/`notifyRust` → `process.stdout.write` / `stderr.write` and
// trips Node's default "throw on unhandled stream error" path. Swallow
// EPIPE everywhere; any other write error still propagates.
for (const stream of [process.stdout, process.stderr]) {
	stream.on("error", (err) => {
		if (err && err.code === "EPIPE") return;
		throw err;
	});
}

// All flags here are part of the **internal** wire between pillbox and
// this helper — the user never types them. If you're reading this from
// the cache (`~/.pillbox/cache/`), DON'T run it directly; the pillbox
// binary owns the blob-file lifecycle and stderr handshake.
function parseArgs(argv) {
	const mode = argv[0];
	if (!mode || (mode !== "attach" && mode !== "reattach" && mode !== "kill")) {
		fail(
			`unsupported mode: ${mode ?? "(none)"} (expected: attach | reattach | kill — this helper is invoked by the pillbox binary, not directly)`,
		);
	}
	const out = {
		mode,
		template: null,
		name: null,
		blobFile: null,
		detach: false,
		sandboxId: null,
		sessionId: null,
		eventsWebhook: null,
		parentSessionId: null,
	};
	for (let i = 1; i < argv.length; i++) {
		const flag = argv[i];
		const val = argv[i + 1];
		switch (flag) {
			case "--template":
				out.template = val;
				i++;
				break;
			case "--name":
				out.name = val;
				i++;
				break;
			case "--blob-file":
				out.blobFile = val;
				i++;
				break;
			case "--detach":
				out.detach = true;
				break;
			case "--sandbox-id":
				out.sandboxId = val;
				i++;
				break;
			case "--session-id":
				// Host pre-mints the id. attach: baked into the sandbox-side
				// `pillbox session done <id>` wrapper AND used to derive the
				// pty-host socket path. reattach: re-derives the same socket
				// path to reconnect a fresh relay. Validated alphanumeric-only
				// on the host side (`Session::new_id` produces hex), so safe to
				// drop into the shell wrapper / socket path without escaping.
				out.sessionId = val;
				i++;
				break;
			case "--events-webhook":
				// Forwarded to the sandbox env so the wrapper's
				// `pillbox session done` can POST the terminal event back. URL
				// is validated at the host CLI level
				// (`validate_events_webhook_url`) before reaching here; we
				// still `shellEscape` on the wrapper-line side as defense in
				// depth.
				out.eventsWebhook = val;
				i++;
				break;
			case "--parent":
				// Forwarded to the sandbox env so the wrapper's
				// `pillbox session started` picks it up (host's CLI already
				// shape-validated via `validate_session_id`). `shellEscape`d
				// again at splice time as defense-in-depth.
				out.parentSessionId = val;
				i++;
				break;
			default:
				fail(`unknown flag: ${flag}`);
		}
	}
	if (mode === "attach") {
		if (!out.template) fail("--template is required for `attach`");
		if (!out.blobFile) fail("--blob-file is required for `attach`");
		if (!out.sessionId) fail("--session-id is required for `attach`");
	}
	if (mode === "reattach") {
		if (!out.sandboxId) fail("--sandbox-id is required for `reattach`");
		if (!out.sessionId) fail("--session-id is required for `reattach`");
	}
	if (mode === "kill") {
		if (!out.sandboxId) fail("--sandbox-id is required for `kill`");
	}
	return out;
}

/// Bourne-shell single-quote escape. POSIX shells treat single-quoted
/// strings literally except for `'` itself, which we close-out, switch
/// to a literal `"'"`, and re-open. Used on every user-influenced value
/// spliced into the PTY launch line below. The Rust host already
/// validates URLs + session ids before they reach here, but defense-in-
/// depth: if a `'` ever sneaks through, we want the wrapper to fail to
/// parse cleanly, not silently execute injected commands.
///
/// Mirrors `shellEscape` in lum's `apps/desktop/scripts/e2b-provider.mjs`
/// — the two helpers diverged from a shared spike, and keeping the
/// idiom byte-identical makes drift easy to spot.
function shellEscape(value) {
	return `'${String(value).replaceAll("'", `'"'"'`)}'`;
}

async function readBlob(path) {
	try {
		return await readFile(path);
	} catch (e) {
		fail(`read blob file ${path}: ${e?.message ?? e}`);
	}
}

/// Deterministic pty-host socket path for a session. The host pre-mints
/// the session id and both `attach` (launches the pty-host here) and
/// `reattach` (connects a fresh relay here) derive the same path — so the
/// session record needs no socket/pid bookkeeping to reconnect.
function sockForSession(sessionId) {
	return `/tmp/pillbox-pty-${sessionId}.sock`;
}

/// Connect to an existing sandbox by id (used by reattach + kill modes).
async function connectSandbox(sandboxId) {
	try {
		return await Sandbox.connect(sandboxId, { timeoutMs: SANDBOX_TIMEOUT_MS });
	} catch (e) {
		fail(`Sandbox.connect(${sandboxId}) failed: ${e?.message ?? e}`);
	}
}

/// The sandbox-side wrapper around `pillbox run`. pty-host runs this as
/// its PTY child (`bash -lc <wrapper>`); pty-host owns the PTY and serves
/// frames, so the wrapper never touches `stty`/markers — that's the
/// relay's job, not the agent's. After the agent exits the wrapper calls
/// `pillbox session done` so the terminal event reaches the configured
/// sinks (webhook in particular — a detached run has no other path home),
/// then `exit "$PB_EXIT"` so the pty-host's child-exit (and thus the
/// Exit frame the host pump reads) carries the AGENT's status, not the
/// trailing `rm`'s.
function buildWrapper(args, blobRemote, resultRemote) {
	const sessionIdEsc = shellEscape(args.sessionId);
	const webhookExport = args.eventsWebhook
		? `export PILLBOX_EVENTS_WEBHOOK=${shellEscape(args.eventsWebhook)}; `
		: "";
	const parentExport = args.parentSessionId
		? `export PILLBOX_PARENT_SESSION_ID=${shellEscape(args.parentSessionId)}; `
		: "";
	// Forward the operator's OTLP config off our inherited env so the
	// sandbox-side tailer (spawned by `pillbox run --vault-stdin-direct`)
	// emits spans straight to their collector. Keep this list in sync with
	// FORWARDED_OTEL_VARS in src/sandbox/remote_ssh.rs. Reachability of the
	// collector from the sandbox is the operator's responsibility.
	const otelExport = OTEL_FORWARD_VARS.filter((k) => process.env[k])
		.map((k) => `export ${k}=${shellEscape(process.env[k])}; `)
		.join("");
	return (
		// PILLBOX_SANDBOX_SIDE flips emitter detection so events render with
		// `emitter=sandbox`. See SANDBOX_SIDE_ENV docs in src/events/mod.rs.
		`export PILLBOX_SANDBOX_SIDE=1; ` +
		// Captured ONCE here via `date -u -Iseconds` (POSIX RFC3339); both
		// `session started` and `session done` read it so their timestamps
		// agree (no skew between the two read sites). If `date` is missing
		// the export expands empty and pillbox falls back to `now_rfc3339`.
		`export PILLBOX_SESSION_STARTED_AT="$(date -u -Iseconds 2>/dev/null)"; ` +
		`${webhookExport}` +
		`${parentExport}` +
		`${otelExport}` +
		`export PILLBOX_RESULT_SNAPSHOT_FILE=${shellEscape(resultRemote)}; ` +
		`rm -f ${shellEscape(resultRemote)}; ` +
		`pillbox session started ${sessionIdEsc}; ` +
		// `--vault-stdin-direct`: the e2b sandbox IS the isolation
		// boundary, so the in-sandbox bootstrap materializes the forwarded
		// agent auth + hydrates the workspace + execs the agent DIRECTLY
		// (no nested Docker, no pre-existing remote login). The SSH path
		// uses the docker-shelled `--vault-stdin` sibling.
		`pillbox run --vault-stdin-direct < ${shellEscape(blobRemote)}; ` +
		`PB_EXIT=$?; ` +
		`RESULT_SNAPSHOT=$(cat ${shellEscape(resultRemote)} 2>/dev/null || true); ` +
		`pillbox session done ${sessionIdEsc} ` +
		`--status "$([ $PB_EXIT = 0 ] && echo ok || echo failed)" ` +
		`--exit-code "$PB_EXIT" ` +
		`--reason "$([ $PB_EXIT = 0 ] && echo agent-completed || echo "agent exited $PB_EXIT")" ` +
		`$([ -n "$RESULT_SNAPSHOT" ] && echo --result-snapshot "$RESULT_SNAPSHOT"); ` +
		`rm -f ${shellEscape(blobRemote)} ${shellEscape(resultRemote)}; ` +
		`exit "$PB_EXIT"`
	);
}

/// Launch the in-sandbox pty-host in the background and wait for its
/// socket to appear. `commands.run({background})` returns as soon as the
/// process starts; the socket-readiness poll is what tells us the
/// pty-host actually came up (vs. e.g. `pillbox` missing from the
/// template). Returns true if the socket appeared within the timeout.
/// THROWS on a `commands.run` failure (rather than `fail()`-exiting) so
/// the caller's teardown path runs — otherwise a launch error would
/// `process.exit` with the just-created sandbox still alive.
async function launchPtyHost(sandbox, sock, wrapper) {
	const cmd =
		`pillbox pty-host --sock ${shellEscape(sock)} ` +
		`-- bash -lc ${shellEscape(wrapper)}`;
	await sandbox.commands.run(cmd, { background: true });
	return waitForSock(sandbox, sock, 15_000);
}

async function waitForSock(sandbox, sock, timeoutMs) {
	const deadline = Date.now() + timeoutMs;
	const test = `test -S ${shellEscape(sock)}`;
	while (Date.now() < deadline) {
		try {
			const r = await sandbox.commands.run(test);
			if (r.exitCode === 0) return true;
		} catch {
			// commands.run throws on non-zero exit — socket not there yet.
		}
		await new Promise((res) => setTimeout(res, 150));
	}
	return false;
}

/// Race `p` against a `ms` timeout so a hung network call can't wedge the
/// helper — and thus the host's `child.wait()` — forever. Errors and
/// timeouts are swallowed: teardown is best-effort by nature.
function withTimeout(p, ms) {
	return Promise.race([
		Promise.resolve(p).catch(() => {}),
		new Promise((res) => setTimeout(res, ms)),
	]);
}

/// A teardown context owns the sandbox lifecycle for one helper run. It's
/// created right after the sandbox exists so its SIGTERM/SIGINT handlers
/// cover the ENTIRE run — including the launch window before any relay —
/// so a stray signal (e.g. `--detach` Ctrl-C, which the host's raw-mode
/// pump isn't there to swallow) can't orphan a billable sandbox.
///
///   - `killSandbox`: true for a foreground `attach` (ephemeral — nuke on
///     exit); false for `reattach` (the run owns the sandbox, we only kill
///     our own relay PTY so no orphan relay lingers).
///   - `keepAlive`: flips true on `--detach` success so teardown leaves
///     the pty-host + agent running for a later `session attach`.
///   - `relayPid`: set once a relay PTY exists; teardown kills it.
function makeSession(sandbox, { killSandbox }) {
	const ctx = { sandbox, killSandbox, keepAlive: false, relayPid: null, tearing: false };
	ctx.teardown = async (code) => {
		if (ctx.tearing) return;
		ctx.tearing = true;
		if (ctx.relayPid !== null) {
			await withTimeout(sandbox.pty.kill(ctx.relayPid), TEARDOWN_TIMEOUT_MS);
		}
		if (ctx.killSandbox && !ctx.keepAlive) {
			await withTimeout(sandbox.kill(), TEARDOWN_TIMEOUT_MS);
		}
		process.exit(code);
	};
	process.on("SIGTERM", () => void ctx.teardown(143));
	process.on("SIGINT", () => void ctx.teardown(130));
	return ctx;
}

/// Connect a `pillbox pty-relay` over a raw E2B PTY and shuttle frames
/// between it and this process's stdio until the session ends, then tear
/// down. The host pump (running over our stdio) owns the protocol, raw
/// mode, resize, and Ctrl-A; we only carry bytes verbatim and drop the
/// relay's cooked-echo preamble up to RELAY_MARKER. Never returns — it
/// `process.exit`s via `teardown`.
async function streamRelay(ctx, sock) {
	const { sandbox } = ctx;
	let booted = false;
	let pre = Buffer.alloc(0);
	const stdinBacklog = [];
	const sendInput = (buf) => void sandbox.pty.sendInput(ctx.relayPid, buf).catch(() => {});
	const flushBacklog = () => {
		while (stdinBacklog.length > 0) sendInput(stdinBacklog.shift());
	};

	let handle;
	try {
		handle = await sandbox.pty.create({
			cols: 80,
			rows: 24,
			// No cwd: the relay just execs `pillbox pty-relay` over the
			// socket, so its working dir is irrelevant — and E2B runs the
			// sandbox as a non-root `user` (uid 1000), so a hardcoded
			// `/root` (mode 700) fails the pty spawn with EACCES.
			envs: { TERM: "xterm-256color", COLORTERM: "truecolor", LANG: "C.UTF-8" },
			timeoutMs: 0,
			onData: (data) => {
				const buf = Buffer.isBuffer(data) ? data : Buffer.from(data);
				if (booted) {
					process.stdout.write(buf);
					return;
				}
				pre = pre.length > 0 ? Buffer.concat([pre, buf]) : buf;
				const idx = pre.indexOf(RELAY_MARKER);
				if (idx === -1) {
					// Bound memory: the marker is 6 bytes, so a tail of a few
					// bytes is enough to catch one straddling a chunk boundary.
					if (pre.length > 4096) pre = pre.subarray(pre.length - 8);
					return;
				}
				booted = true;
				const tail = pre.subarray(idx + RELAY_MARKER.length);
				pre = Buffer.alloc(0);
				// Relay is live now — release any frames the host pump sent
				// (e.g. the opening Hello) while the PTY shell was still
				// booting, in order.
				flushBacklog();
				if (tail.length > 0) process.stdout.write(tail);
			},
		});
	} catch (e) {
		process.stderr.write(`pillbox-e2b-helper: pty.create (relay) failed: ${e?.message ?? e}\n`);
		await ctx.teardown(1);
		return;
	}
	ctx.relayPid = handle.pid;

	// stdin (host pump frames) → relay PTY, verbatim. Buffer until the
	// relay marker so nothing is delivered to a half-booted shell.
	process.stdin.on("data", (chunk) => {
		const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
		if (!booted) {
			stdinBacklog.push(buf);
			return;
		}
		sendInput(buf);
	});
	process.stdin.on("end", () => void ctx.teardown(0));

	// Kick the relay: raw mode, emit the marker, then exec the relay so it
	// inherits the PTY's stdio (the shell is replaced, not forked).
	const launch =
		`stty -echo raw 2>/dev/null; printf '\\036\\036PB\\036\\036'; ` +
		`exec pillbox pty-relay --sock ${shellEscape(sock)}\n`;
	try {
		await sandbox.pty.sendInput(ctx.relayPid, Buffer.from(launch));
	} catch (e) {
		process.stderr.write(`pillbox-e2b-helper: relay launch failed: ${e?.message ?? e}\n`);
		await ctx.teardown(1);
		return;
	}

	try {
		await handle.wait();
	} catch {
		// Relay PTY ended (peer death / agent exit) — fall through to
		// teardown; the host pump has already resolved its own outcome.
	}
	await ctx.teardown(0);
}

/// Mode: `attach` — create a sandbox, stage the blob, launch the pty-host
/// (which runs the agent under its PTY), then either stream frames back
/// via a relay (interactive) or exit after launch (`--detach`).
async function runAttach(args) {
	const blobBytes = await readBlob(args.blobFile);

	let sandbox;
	try {
		sandbox = await Sandbox.create(args.template, {
			timeoutMs: SANDBOX_TIMEOUT_MS,
			metadata: args.name ? { pillboxRemote: args.name } : undefined,
		});
	} catch (e) {
		fail(`Sandbox.create failed: ${e?.message ?? e}`);
	}

	// Own the sandbox lifecycle from here. `killSandbox: true` — a
	// foreground run is ephemeral and writes no record, so any exit path
	// (signal, error, or clean finish) must tear the sandbox down.
	const ctx = makeSession(sandbox, { killSandbox: true });

	try {
		const blobName = `pillbox-blob-${randomBytes(6).toString("hex")}.json`;
		const blobRemote = `/tmp/${blobName}`;
		await sandbox.files.write(blobRemote, blobBytes);

		const resultRemote = `/tmp/pillbox-result-${args.sessionId}.txt`;
		const sock = sockForSession(args.sessionId);
		const wrapper = buildWrapper(args, blobRemote, resultRemote);

		const up = await launchPtyHost(sandbox, sock, wrapper);
		if (!up) {
			process.stderr.write(
				`pillbox-e2b-helper: pty-host did not come up (socket ${sock} never appeared). ` +
					"Is `pillbox` baked into the template image?\n",
			);
			await ctx.teardown(1);
			return;
		}

		notifyRust({
			type: "sandbox-up",
			protoVersion: PROTO_VERSION,
			sandboxId: sandbox.sandboxId,
		});

		if (args.detach) {
			// Detached: the pty-host + agent are running headless. Leave the
			// sandbox up (the host records the session; `pillbox session
			// attach <id>` connects a relay later) and exit.
			ctx.keepAlive = true;
			notifyRust({ type: "detached" });
			process.exit(0);
		}

		await streamRelay(ctx, sock);
	} catch (e) {
		process.stderr.write(`pillbox-e2b-helper: attach failed: ${e?.message ?? e}\n`);
		await ctx.teardown(1);
	}
}

/// Mode: `reattach` — connect to an existing sandbox and stream a fresh
/// relay to the still-running pty-host (socket derived from the session
/// id). `killSandbox: false` — the run owns the sandbox; teardown only
/// kills our own relay PTY.
async function runReattach(args) {
	const sandbox = await connectSandbox(args.sandboxId);
	const sock = sockForSession(args.sessionId);
	const ctx = makeSession(sandbox, { killSandbox: false });

	notifyRust({
		type: "sandbox-up",
		protoVersion: PROTO_VERSION,
		sandboxId: args.sandboxId,
	});

	try {
		await streamRelay(ctx, sock);
	} catch (e) {
		process.stderr.write(`pillbox-e2b-helper: reattach failed: ${e?.message ?? e}\n`);
		await ctx.teardown(1);
	}
}

/// Mode: `kill` — tear the sandbox down. Stand-alone: no PTY, no stdio
/// shuttling. Used by `pillbox session rm <id>`.
async function runKill(args) {
	const sandbox = await connectSandbox(args.sandboxId);
	try {
		await sandbox.kill();
	} catch (e) {
		fail(`sandbox.kill failed: ${e?.message ?? e}`);
	}
	notifyRust({ type: "sandbox-up", protoVersion: PROTO_VERSION, sandboxId: args.sandboxId });
	process.exit(0);
}

async function main() {
	const args = parseArgs(process.argv.slice(2));
	if (!process.env.E2B_API_KEY) {
		fail(
			"E2B_API_KEY is not set in the helper environment. " +
				"Set it locally (e.g. via `pillbox secret add E2B_API_KEY ...` and export it) or pass it through `pillbox run`'s env.",
		);
	}
	switch (args.mode) {
		case "attach":
			await runAttach(args);
			break;
		case "reattach":
			await runReattach(args);
			break;
		case "kill":
			await runKill(args);
			break;
	}
}

main().catch((e) => {
	process.stderr.write(`${e?.stack ?? e}\n`);
	process.exit(1);
});
