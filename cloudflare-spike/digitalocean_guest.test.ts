import assert from "node:assert/strict";
import { mkdtemp, writeFile, chmod, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { test } from "node:test";
import { DIGITALOCEAN_GUEST, DIGITALOCEAN_LAUNCH } from "./src/digitalocean_guest.ts";
import { openRouterReasoning } from "./src/opencode_reasoning.ts";
const exec = promisify(execFile);

const cases = [
  { effort: "low", schema: false, nativeError: "", rejected: false },
  { effort: "medium", schema: false, nativeError: "", rejected: false },
  { effort: "high", schema: false, nativeError: "", rejected: false },
  { effort: "high", schema: true, nativeError: "StructuredOutputError", rejected: false },
  { effort: "high", schema: true, nativeError: "APIError", rejected: true },
  { effort: "high", schema: false, nativeError: "StructuredOutputError", rejected: true },
] as const;
for (const { effort, schema, nativeError, rejected } of cases)
  test(`guest: ${effort}, schema=${schema}, native error=${nativeError || "none"}`, async () => {
    const dir = await mkdtemp(join(tmpdir(), "pillbox-do-guest-"));
    const resultPath = join(dir, "result.json");
    const fake = join(dir, "opencode");
    await writeFile(
      fake,
      `#!/usr/bin/env python3
import sys, json, os
if "--version" in sys.argv:
    print("1.18.31")
    sys.exit(0)
from http.server import BaseHTTPRequestHandler, HTTPServer
assert "DIGITALOCEAN_API_TOKEN" not in os.environ
config = json.loads(os.environ["OPENCODE_CONFIG_CONTENT"])
assert config["permission"] == "deny"
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b'{"healthy":true}')
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/session":
            assert body["permission"][0]["action"] == "deny"
            value = {"id":"session1"}
        else:
            assert body["tools"]["bash"] is False
            assert body["variant"] == "pillbox-${effort}"
            variant = config["provider"]["openrouter"]["models"][body["model"]["modelID"]]["variants"][body["variant"]]
            assert variant == {"reasoning": {"effort": "${effort}"}, "provider": {"require_parameters": True}}
            assert body["model"] == {"providerID":"openrouter","modelID":"openai/gpt-5-mini"}
            value = {"info":{"id":"msg1","role":"assistant","providerID":"openrouter","modelID":"openai/gpt-5-mini","finish":"stop","time":{"completed":1},"tokens":{"input":1,"output":1,"cache":{"read":0,"write":0}},"cost":0.001},"parts":[{"type":"text","text":${JSON.stringify(schema ? '{"ok":true}' : "NATIVE-COMPLETE")}}]}
            if "${nativeError}": value["info"]["error"] = {"name": "${nativeError}"}
        self.send_response(200); self.end_headers(); self.wfile.write(json.dumps(value).encode())
HTTPServer(("127.0.0.1",4197),Handler).serve_forever()
`,
    );
    await chmod(fake, 0o755);
    const payload = JSON.stringify({
      harness_version: "1.18.31",
      text: "test",
      model: "openai/gpt-5-mini",
      reasoning: openRouterReasoning({
        provider: "openrouter",
        model: "openai/gpt-5-mini",
        profile: null,
        reasoning_effort: effort,
      }),
      tools: { bash: false },
      output_format: schema
        ? {
            type: "json_schema",
            retry_count: 2,
            schema: { type: "object", required: ["ok"], properties: { ok: { type: "boolean" } } },
          }
        : { type: "text", retry_count: 0 },
    });
    try {
      const launched = await exec(
        "python3",
        ["-c", DIGITALOCEAN_LAUNCH, DIGITALOCEAN_GUEST, payload, resultPath],
        {
          env: {
            ...process.env,
            PATH: `${dir}:${process.env.PATH}`,
            OPENROUTER_API_KEY: "synthetic-test-key",
            DIGITALOCEAN_API_TOKEN: "must-not-reach-model-process",
          },
          timeout: 5000,
        },
      );
      assert.equal(launched.stdout.trim(), "started");
      let result: Record<string, unknown> | undefined;
      for (let i = 0; i < 100; i++) {
        try {
          result = JSON.parse(await readFile(resultPath, "utf8"));
          break;
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
        }
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
      assert.ok(result, "guest must finish independently of the long-running server");
      if (rejected) {
        assert.deepEqual(result, { error: "native_result_validation" });
      } else {
        assert.equal(result.text, schema ? '{"ok":true}' : "NATIVE-COMPLETE");
        assert.equal(result.harness_version, "1.18.31");
      }
      await assert.rejects(
        exec("python3", ["-c", DIGITALOCEAN_LAUNCH, DIGITALOCEAN_GUEST, payload, resultPath], {
          timeout: 5000,
        }),
        /File exists/,
      );
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
