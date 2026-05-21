#!/usr/bin/env node
//
// pillbox-e2b-helper — Node subprocess that bridges local pillbox to an
// E2B sandbox PTY.
//
// Wire (set by the Rust caller in src/sandbox/remote_e2b.rs):
//   - argv:  attach --template TEMPLATE_ID [--name NAME] [--blob-file PATH]
//   - env:   E2B_API_KEY=...   (read by the @e2b/code-interpreter SDK)
//   - stdin: forwarded to the sandbox PTY as user keystrokes (once the
//            in-sandbox `pillbox run --vault-stdin` has consumed its blob)
//   - stdout: PTY output → local terminal
//   - stderr: helper diagnostics + a one-line JSON handshake
//            `{type:"sandbox-up", protoVersion, sandboxId}` so the Rust
//            side can verify the protocol and surface the sandbox id
//            (see `pump_helper_stderr` + `HELPER_PROTO_VERSION` in
//            `src/sandbox/remote_e2b.rs`; bump both together).
//
// The vault-stdin blob is written to a temp file inside the sandbox via
// the E2B Files API (mode 600, unlinked after pillbox consumes it). This
// keeps secret bytes off the PTY — which by default echoes everything —
// and lets the in-sandbox pillbox read its stdin to EOF like the SSH path
// already does. /tmp inside an E2B sandbox is per-microVM and discarded
// at sandbox.kill().
//
// Why a Node helper and not a Rust HTTP client: E2B publishes no official
// Rust SDK and the existing third-party crate covers only code-interpreter
// (no PTY, no commands.run). Porting the SDK protocol natively is ~1.5K
// LOC; the JS SDK is the supported surface. This file is embedded into
// the pillbox binary via include_str! and written to a cache path on
// first use — users still need `node` + `npm i -g e2b` available.

import { Sandbox } from "e2b";
import { readFile } from "node:fs/promises";
import { randomBytes } from "node:crypto";

const PROTO_VERSION = 1;
const SANDBOX_TIMEOUT_MS = 3_600_000;
const BOOT_MARKER = "__PILLBOX_BOOT__";

function fail(msg) {
	process.stderr.write(`pillbox-e2b-helper: ${msg}\n`);
	process.exit(1);
}

// All flags here are part of the **internal** wire between pillbox and
// this helper — the user never types them. Pillbox always invokes us as
//   node helper.mjs attach --template T --blob-file F [--name N]
// If you're reading this from the cache (`~/.pillbox/cache/`), DON'T run
// it directly — the blob file is a one-shot 0600 stage produced by the
// pillbox binary; nothing else writes that shape.
function parseArgs(argv) {
	if (argv[0] !== "attach") {
		fail(`unsupported mode: ${argv[0] ?? "(none)"} (expected: attach — this helper is invoked by the pillbox binary, not directly)`);
	}
	const out = { template: null, name: null, blobFile: null };
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
				// Internal: path to a 0600 temp file produced by the
				// pillbox binary in `run_via_helper`. Not a user flag.
				out.blobFile = val;
				i++;
				break;
			default:
				fail(`unknown flag: ${flag}`);
		}
	}
	if (!out.template) fail("--template is required");
	if (!out.blobFile) fail("--blob-file is required");
	return out;
}

function notifyRust(payload) {
	// Rust reads a single line of JSON from stderr to learn the sandbox
	// id (for the connect-message + tear-down on Ctrl-C). Using stderr
	// keeps stdout pure PTY output.
	process.stderr.write(`${JSON.stringify(payload)}\n`);
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

async function main() {
	if (!process.env.E2B_API_KEY) {
		fail(
			"E2B_API_KEY is not set in the helper environment. " +
				"Set it locally (e.g. via `pillbox secret add E2B_API_KEY ...` and export it) or pass it through `pillbox run`'s env.",
		);
	}
	const args = parseArgs(process.argv.slice(2));
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

	notifyRust({ type: "sandbox-up", protoVersion: PROTO_VERSION, sandboxId: sandbox.sandboxId });

	let cleaning = false;
	const cleanup = async (code = 0) => {
		if (cleaning) return;
		cleaning = true;
		try {
			await sandbox.kill();
		} catch {
			// best-effort
		}
		process.exit(code);
	};
	process.on("SIGTERM", () => void cleanup(143));
	process.on("SIGINT", () => void cleanup(130));

	// Stage the blob inside the sandbox. Random suffix keeps multiple
	// concurrent runs (different `pillbox run --remote NAME`) on the same
	// sandbox image from colliding. The in-sandbox shell command will
	// `rm -f` it as soon as `pillbox run --vault-stdin` finishes reading.
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
					// Drop everything up to and including the marker line so
					// the user's terminal doesn't see our handshake.
					const tail = bootBuffer.slice(idx + BOOT_MARKER.length).replace(/^\r?\n/, "");
					bootBuffer = "";
					if (tail.length > 0) process.stdout.write(tail);
					return;
				}
				process.stdout.write(text);
			},
		});
	} catch (e) {
		await cleanup(1);
		fail(`pty.create failed: ${e?.message ?? e}`);
	}
	const pid = handle.pid;

	// Probe + launch sequence — single send so the user doesn't see two
	// echoed prompts:
	//   1. `stty -echo raw 2>/dev/null` — quiet the TTY so the blob path
	//      and trailing rm don't print to the user's terminal.
	//   2. Print BOOT_MARKER so we know stty took effect (it could
	//      reasonably fail if the template ships a non-busybox /bin/sh).
	//   3. exec `pillbox run --vault-stdin < blobPath` — exec replaces
	//      the shell, so when pillbox exits the PTY closes (handle.wait
	//      returns) instead of dropping back to a prompt.
	//   4. The blob file is removed after pillbox finishes reading it.
	const launch =
		`stty -echo raw 2>/dev/null; printf '%s\\n' '${BOOT_MARKER}'; ` +
		`pillbox run --vault-stdin < ${blobRemote}; rm -f ${blobRemote}\n`;
	try {
		await sandbox.pty.sendInput(pid, Buffer.from(launch));
	} catch (e) {
		await cleanup(1);
		fail(`send launch line: ${e?.message ?? e}`);
	}

	// Forward local keystrokes to the sandbox PTY. We start this BEFORE
	// the marker shows up: any keys typed before the agent is ready end
	// up at the shell prompt, which discards them (the launch line above
	// will be the next thing the shell sees once stty is applied).
	process.stdin.setRawMode?.(true);
	process.stdin.on("data", (chunk) => {
		const bytes = chunk instanceof Uint8Array ? chunk : Buffer.from(chunk);
		void sandbox.pty.sendInput(pid, bytes).catch(() => {});
	});
	process.stdin.on("end", () => void cleanup(0));
	process.stdout.on("resize", () => {
		const { cols, rows } = sizeFromStdout();
		void sandbox.pty.resize(pid, { cols, rows }).catch(() => {});
	});

	try {
		await handle.wait();
	} catch (e) {
		if (!cleaning) {
			await cleanup(1);
			fail(`pty wait: ${e?.message ?? e}`);
		}
	}
	await cleanup(0);
}

main().catch((e) => {
	process.stderr.write(`${e?.stack ?? e}\n`);
	process.exit(1);
});
