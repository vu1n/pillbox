// Generates src/contract.ts from contract.schema.json (which src/contract.rs emits).
// `--check` compares in memory and touches no file, so it works on an unstaged or
// git-less tree; CI and scripts/smoke/cf.sh run it.
import { readFile, writeFile } from "node:fs/promises";
import { compile } from "json-schema-to-typescript";

const root = new URL("../", import.meta.url);
const schemaUrl = new URL("contract.schema.json", root);
const outUrl = new URL("src/contract.ts", root);

const BANNER =
  "// GENERATED from contract.schema.json, which src/contract.rs emits. Do not edit; change src/contract.rs, then run\n" +
  "// UPDATE_SCHEMA=1 cargo test --no-default-features --features contract-schema contract_schema_is_current && npm run gen:contract";

const schema = JSON.parse(await readFile(schemaUrl, "utf8"));
const generated = await compile(schema, "Event", {
  bannerComment: BANNER,
  // Rust never emits fields it does not declare, so generated interfaces stay closed.
  additionalProperties: false,
  // serde_json::Value fields become `unknown`, not `any`.
  unknownAny: true,
  cwd: new URL(".", root).pathname,
});

if (process.argv.includes("--check")) {
  const committed = await readFile(outUrl, "utf8").catch(() => "");
  if (committed !== generated) {
    console.error("src/contract.ts is stale: run `npm run gen:contract` in cloudflare-spike/");
    process.exit(1);
  }
} else {
  await writeFile(outUrl, generated);
}
