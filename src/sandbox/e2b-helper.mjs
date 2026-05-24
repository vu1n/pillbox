#!/usr/bin/env node
//
// pillbox-e2b-helper — Node subprocess that bridges local pillbox to an
// E2B sandbox PTY.
//
// Wire (set by the Rust caller in src/sandbox/remote_e2b.rs):
//   - argv (one of):
//       attach   --template TEMPLATE_ID --blob-file PATH [--name N] [--detach]
//       reattach --sandbox-id ID --pid N
//       kill     --sandbox-id ID
//   - env:   E2B_API_KEY=...   (read by the @e2b/code-interpreter SDK)
//   - stdin: forwarded to the sandbox PTY as user keystrokes. Ctrl-A
//            (0x01) is the detach prefix — Ctrl-A D requests detach,
//            Ctrl-A Ctrl-A sends a literal Ctrl-A through to the PTY.
//   - stdout: PTY output → local terminal.
//   - stderr: helper diagnostics + JSON handshake lines.
//
// ## Wire (stderr handshake)
//
// One JSON line per state transition, parsed by `pump_helper_stderr`
// in `src/sandbox/remote_e2b.rs`. Always sent before any free-text
// diagnostics so the Rust side can distinguish protocol from noise.
//
//   {type:"sandbox-up", protoVersion, sandboxId, pid?}
//       Sent after Sandbox.create / pty.create succeed. `pid` is set
//       in attach/reattach modes; absent in `kill`. The Rust side
//       echoes the sandbox id to the user and persists the session
//       record (for `attach --detach` only).
//   {type:"detach-pressed"}
//       Sent by an interactive attach when the user types Ctrl-A D.
//       The Rust side prints "detached. reattach with: pillbox
//       session attach <id>" and exits with success.
//   {type:"detached"}
//       Sent when `attach --detach` finishes its launch (sandbox + PTY
//       up, agent command sent) and is about to exit. Treated the same
//       as `detach-pressed` on the Rust side.
//
// ## Why a temp file (not stdin) for the blob
//
// The PTY echoes input by default and turning echo off only takes
// effect after the shell runs `stty`. If we sent the blob through the
// PTY before then, the user's terminal would briefly display tens of
// kilobytes of JSON (including secret material). Staging to a sandbox
// /tmp file via the Files API keeps secrets off the user-visible
// display path. The file is unlinked by the launch line as soon as
// `pillbox run --vault-stdin` has read it.
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
const BOOT_MARKER = "__PILLBOX_BOOT__";

// Detach hotkey: Ctrl-A (0x01) + D / d. Mirrors GNU screen's default.
// Ctrl-A Ctrl-A sends a literal Ctrl-A through to the PTY so users
// can still use readline's beginning-of-line in shells inside the
// sandbox. Exit code 100 distinguishes intentional detach from
// SIGINT (130) and SIGTERM (143).
const DETACH_PREFIX = 0x01;
const DETACH_KEY_LOWER = 0x64; // 'd'
const DETACH_KEY_UPPER = 0x44; // 'D'
const DETACH_EXIT_CODE = 100;

function fail(msg) {
	process.stderr.write(`pillbox-e2b-helper: ${msg}\n`);
	process.exit(1);
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
		pid: null,
		sessionId: null,
		eventsWebhook: null,
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
			case "--pid":
				out.pid = Number.parseInt(val ?? "", 10);
				i++;
				break;
			case "--session-id":
				// Host pre-mints the id and passes it through so the
				// sandbox-side wrapper can bake it into the
				// `pillbox session done <id>` call after the agent
				// exits. Validated alphanumeric-only on the host side
				// (`Session::new_id` produces hex), so safe to drop
				// into the shell wrapper without further escaping.
				out.sessionId = val;
				i++;
				break;
			case "--events-webhook":
				// Forwarded to the sandbox env so the wrapper's
				// `pillbox session done` can POST the terminal event
				// back. URL is validated at the host CLI level
				// (`validate_events_webhook_url` in src/main.rs:
				// http(s):// scheme, no whitespace / control chars,
				// http-to-non-loopback warns) before reaching here;
				// we still `shellEscape` on the wrapper-line side as
				// defense in depth.
				out.eventsWebhook = val;
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
		if (!Number.isFinite(out.pid)) fail("--pid is required for `reattach`");
	}
	if (mode === "kill") {
		if (!out.sandboxId) fail("--sandbox-id is required for `kill`");
	}
	return out;
}

function notifyRust(payload) {
	process.stderr.write(`${JSON.stringify(payload)}\n`);
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

function sizeFromStdout() {
	const cols = Number.isFinite(process.stdout.columns) ? process.stdout.columns : 80;
	const rows = Number.isFinite(process.stdout.rows) ? process.stdout.rows : 24;
	return { cols: Math.max(1, cols), rows: Math.max(1, rows) };
}

/// Stream stdin → PTY with the Ctrl-A detach prefix interpreted. Returns
/// a function that detaches the listener (used by callers that need to
/// suspend forwarding before exiting).
function wireStdinToPty(sandbox, pid, onDetach) {
	let prefixSeen = false;
	process.stdin.setRawMode?.(true);
	const handler = (chunk) => {
		const bytes = chunk instanceof Uint8Array ? chunk : Buffer.from(chunk);
		const out = [];
		for (const byte of bytes) {
			if (prefixSeen) {
				prefixSeen = false;
				if (byte === DETACH_KEY_LOWER || byte === DETACH_KEY_UPPER) {
					if (out.length > 0) {
						void sandbox.pty.sendInput(pid, Buffer.from(out)).catch(() => {});
					}
					onDetach();
					return;
				}
				// Any other byte after the prefix sends Ctrl-A + that byte
				// literally — `Ctrl-A Ctrl-A` thus delivers a single Ctrl-A
				// through to the sandbox so users keep readline's
				// beginning-of-line / similar bindings.
				out.push(DETACH_PREFIX);
				out.push(byte);
			} else if (byte === DETACH_PREFIX) {
				prefixSeen = true;
			} else {
				out.push(byte);
			}
		}
		if (out.length > 0) {
			void sandbox.pty.sendInput(pid, Buffer.from(out)).catch(() => {});
		}
	};
	process.stdin.on("data", handler);
	return () => {
		process.stdin.off("data", handler);
		process.stdin.setRawMode?.(false);
	};
}

/// Connect to an existing sandbox by id (used by reattach + kill modes).
async function connectSandbox(sandboxId) {
	try {
		return await Sandbox.connect(sandboxId, { timeoutMs: SANDBOX_TIMEOUT_MS });
	} catch (e) {
		fail(`Sandbox.connect(${sandboxId}) failed: ${e?.message ?? e}`);
	}
}

/// Mode: `kill` — tear the sandbox down. Stand-alone: no PTY, no stdin
/// forwarding. Used by `pillbox session rm <id>`.
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

/// Mode: `reattach` — connect to an existing sandbox's PTY by pid and
/// stream like a normal attach. Ctrl-A D detaches without killing.
async function runReattach(args) {
	const sandbox = await connectSandbox(args.sandboxId);
	const size = sizeFromStdout();

	let detaching = false;
	const detach = (reason) => {
		if (detaching) return;
		detaching = true;
		notifyRust({ type: reason });
		// Reattach never owns the sandbox lifecycle — we never call
		// sandbox.kill(). `pillbox session rm <id>` is the explicit
		// teardown path.
		process.exit(DETACH_EXIT_CODE);
	};
	process.on("SIGTERM", () => detach("detach-pressed"));
	process.on("SIGINT", () => detach("detach-pressed"));

	let handle;
	try {
		handle = await sandbox.pty.connect(args.pid, {
			timeoutMs: 0,
			onData: (data) => process.stdout.write(Buffer.from(data)),
		});
	} catch (e) {
		fail(`pty.connect(${args.pid}) failed: ${e?.message ?? e}`);
	}

	notifyRust({
		type: "sandbox-up",
		protoVersion: PROTO_VERSION,
		sandboxId: args.sandboxId,
		pid: args.pid,
	});
	// Re-assert PTY dimensions to match the local terminal — the
	// remote may have been opened from a different-sized client.
	try {
		await sandbox.pty.resize(args.pid, size);
	} catch {
		// non-fatal — the remote PTY may have its own state and
		// resize is a hint anyway.
	}

	wireStdinToPty(sandbox, args.pid, () => detach("detach-pressed"));
	process.stdin.on("end", () => detach("detach-pressed"));
	process.stdout.on("resize", () => {
		const { cols, rows } = sizeFromStdout();
		void sandbox.pty.resize(args.pid, { cols, rows }).catch(() => {});
	});

	try {
		await handle.wait();
	} catch (e) {
		if (!detaching) fail(`pty wait: ${e?.message ?? e}`);
	}
	// Process inside the PTY exited (e.g. user typed `exit`) — the
	// sandbox is empty but still alive. Same as detach: leave it.
	process.exit(0);
}

/// Mode: `attach` — create a new sandbox + PTY, launch
/// `pillbox run --vault-stdin` inside, optionally exit immediately
/// after launch (`--detach`) so the local pillbox can record the
/// session and return.
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

	let cleaning = false;
	let keepAliveOnExit = false;
	const cleanup = async (code = 0) => {
		if (cleaning) return;
		cleaning = true;
		if (!keepAliveOnExit) {
			try {
				await sandbox.kill();
			} catch {
				// best-effort
			}
		}
		process.exit(code);
	};
	process.on("SIGTERM", () => void cleanup(143));
	process.on("SIGINT", () => void cleanup(130));

	// Stage the blob inside the sandbox.
	const blobName = `pillbox-blob-${randomBytes(6).toString("hex")}.json`;
	const blobRemote = `/tmp/${blobName}`;
	try {
		await sandbox.files.write(blobRemote, blobBytes);
	} catch (e) {
		await cleanup(1);
		fail(`write vault blob into sandbox: ${e?.message ?? e}`);
	}

	const size = sizeFromStdout();
	let booted = false;
	let bootBuffer = "";

	let handle;
	try {
		handle = await sandbox.pty.create({
			...size,
			cwd: "/root",
			envs: { TERM: "xterm-256color", COLORTERM: "truecolor", LANG: "C.UTF-8" },
			timeoutMs: 0,
			onData: (data) => {
				const text = Buffer.from(data).toString();
				if (!booted) {
					bootBuffer += text;
					const idx = bootBuffer.indexOf(BOOT_MARKER);
					if (idx === -1) return;
					booted = true;
					const tail = bootBuffer.slice(idx + BOOT_MARKER.length).replace(/^\r?\n/, "");
					bootBuffer = "";
					if (tail.length > 0 && !args.detach) process.stdout.write(tail);
					return;
				}
				if (!args.detach) process.stdout.write(text);
			},
		});
	} catch (e) {
		await cleanup(1);
		fail(`pty.create failed: ${e?.message ?? e}`);
	}
	const pid = handle.pid;

	notifyRust({
		type: "sandbox-up",
		protoVersion: PROTO_VERSION,
		sandboxId: sandbox.sandboxId,
		pid,
	});

	// Build the wrapper around `pillbox run --vault-stdin`. After the
	// agent exits, capture its exit code and call `pillbox session
	// done` with the pre-minted id so the terminal event (completed /
	// failed) reaches whatever sinks the env exposes — webhook in
	// particular, since detached runs have no other path back to the
	// host. The webhook URL is `shellEscape`d defensively even though
	// the host validates URL shape; if the URL ever contained shell
	// metacharacters we'd want to know via clean fail, not via
	// surprise command injection.
	const sessionIdEsc = shellEscape(args.sessionId);
	const webhookExport = args.eventsWebhook
		? `export PILLBOX_EVENTS_WEBHOOK=${shellEscape(args.eventsWebhook)}; `
		: "";
	// After the agent exits, snapshot the modified workspace into the
	// shared rustic repo (`pillbox push --tag session-<id> --json`) and
	// extract the snapshot handle from the JSON output. The handle gets
	// passed to `pillbox session done --result-snapshot HANDLE` so the
	// host record can be updated by an orchestrator (or by a manual
	// `session done` invocation on the host).
	//
	// `jq -r .snapshot.handle` is the canonical extraction — pillbox's
	// own `--json` output keeps that shape stable. `2>/dev/null` on the
	// push hides the human banner (we already have JSON); if push or
	// jq fail, `RESULT_SNAPSHOT` stays empty and `--result-snapshot`
	// is dropped from the `session done` call (the if-non-empty guard
	// at the bottom of the line). That keeps the failure path clean —
	// terminal event still fires, just without a result_snapshot.
	// PILLBOX_SANDBOX_SIDE flips the emitter detection so events
	// render with `emitter=sandbox`. Set once at the top so every
	// pillbox call below picks it up. See SANDBOX_SIDE_ENV docs in
	// src/events/mod.rs for the trust-model rationale.
	//
	// `pillbox session started` fires immediately after the export so
	// the timestamp reflects sandbox-side-ready, not host-side-saw-
	// handshake. The delta between this event and the host's
	// `session.started` is the cold-start latency consumers care
	// about.
	const launch =
		`stty -echo raw 2>/dev/null; printf '%s\\n' '${BOOT_MARKER}'; ` +
		`export PILLBOX_SANDBOX_SIDE=1; ` +
		`${webhookExport}` +
		`pillbox session started ${sessionIdEsc}; ` +
		`pillbox run --vault-stdin < ${shellEscape(blobRemote)}; ` +
		`PB_EXIT=$?; ` +
		`RESULT_SNAPSHOT=$(pillbox push --tag ${shellEscape(`session-${args.sessionId}`)} --message ${shellEscape(`agent result for session ${args.sessionId}`)} --json 2>/dev/null | jq -r '.snapshot.handle // empty' 2>/dev/null); ` +
		`pillbox session done ${sessionIdEsc} ` +
		`--status "$([ $PB_EXIT = 0 ] && echo ok || echo failed)" ` +
		`--exit-code "$PB_EXIT" ` +
		`--reason "$([ $PB_EXIT = 0 ] && echo agent-completed || echo "agent exited $PB_EXIT")" ` +
		`$([ -n "$RESULT_SNAPSHOT" ] && echo --result-snapshot "$RESULT_SNAPSHOT"); ` +
		`rm -f ${shellEscape(blobRemote)}\n`;
	try {
		await sandbox.pty.sendInput(pid, Buffer.from(launch));
	} catch (e) {
		await cleanup(1);
		fail(`send launch line: ${e?.message ?? e}`);
	}

	if (args.detach) {
		// Detached mode: the sandbox + PTY are up, the agent command is
		// in the PTY's stdin. We exit without killing — the caller
		// (`pillbox run --remote NAME --detach`) writes the session
		// record from the handshake. `pillbox session attach <id>` is
		// the reconnect path.
		keepAliveOnExit = true;
		notifyRust({ type: "detached" });
		process.exit(0);
	}

	let detaching = false;
	const detach = () => {
		if (detaching) return;
		detaching = true;
		keepAliveOnExit = true;
		notifyRust({ type: "detach-pressed" });
		process.exit(DETACH_EXIT_CODE);
	};

	wireStdinToPty(sandbox, pid, detach);
	process.stdin.on("end", () => void cleanup(0));
	process.stdout.on("resize", () => {
		const { cols, rows } = sizeFromStdout();
		void sandbox.pty.resize(pid, { cols, rows }).catch(() => {});
	});

	try {
		await handle.wait();
	} catch (e) {
		if (!cleaning && !detaching) {
			await cleanup(1);
			fail(`pty wait: ${e?.message ?? e}`);
		}
	}
	await cleanup(0);
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
