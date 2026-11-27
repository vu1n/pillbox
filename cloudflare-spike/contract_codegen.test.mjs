// check:contract must fail on a stale src/contract.ts without rewriting it, and pass on a current one.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { cpSync, mkdtempSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const here = fileURLToPath(new URL(".", import.meta.url));

function copyTree() {
  const dir = mkdtempSync(join(tmpdir(), "contract-codegen-"));
  mkdirSync(join(dir, "scripts"));
  mkdirSync(join(dir, "src"));
  cpSync(join(here, "scripts/gen-contract.mjs"), join(dir, "scripts/gen-contract.mjs"));
  cpSync(join(here, "contract.schema.json"), join(dir, "contract.schema.json"));
  cpSync(join(here, "src/contract.ts"), join(dir, "src/contract.ts"));
  symlinkSync(join(here, "node_modules"), join(dir, "node_modules"));
  return dir;
}

function check(dir) {
  return spawnSync(process.execPath, [join(dir, "scripts/gen-contract.mjs"), "--check"], { encoding: "utf8" });
}

test("the committed contract.ts is current", () => {
  const dir = copyTree();
  try {
    assert.equal(check(dir).status, 0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a hand edit to contract.ts fails the check and is left in place", () => {
  const dir = copyTree();
  try {
    const out = join(dir, "src/contract.ts");
    const edited = readFileSync(out, "utf8") + "export type HandEdit = 1;\n";
    writeFileSync(out, edited);
    assert.equal(check(dir).status, 1);
    assert.equal(readFileSync(out, "utf8"), edited);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a schema change without regenerating fails the check", () => {
  const dir = copyTree();
  try {
    const schemaPath = join(dir, "contract.schema.json");
    const schema = JSON.parse(readFileSync(schemaPath, "utf8"));
    schema.$defs.ResultReady.properties.addedField = { type: "string" };
    writeFileSync(schemaPath, JSON.stringify(schema));
    assert.equal(check(dir).status, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
