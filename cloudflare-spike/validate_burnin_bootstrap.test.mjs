import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import {
  D1_BOOTSTRAP_QUERY,
  parseWranglerToml,
  validateBurninBootstrap as validateBurninBootstrapRaw,
} from "./scripts/validate-burnin-bootstrap.mjs";

const publicKey = "ed25519:SKaA_nE69844nq00przjwmcPcg5iRY1hL4fnfgipY94";
const fingerprint = `sha256:${createHash("sha256")
  .update(Buffer.from(publicKey.slice("ed25519:".length), "base64url"))
  .digest("hex")}`;

const bootstrapBase = {
  schema_version: "huddles.pillbox-burnin-bootstrap/1",
  mode: "apply",
  installation: {
    installation_id: "pillbox-burnin-installation-1",
    execution_realm_id: "pillbox-managed-burnin-1",
    protocol_revision: "pillbox.huddles/1",
    organization_id: "pillbox-burnin-organization-1",
    workspace_id: "pillbox-burnin-workspace-1",
    principal_id: "pillbox-burnin-principal-1",
    policy_id: "pillbox-burnin-policy-1",
  },
  signing_key: {
    key_id: "huddles-pillbox-grant-burnin-1",
    public_key: publicKey,
    fingerprint,
  },
  services: {
    issuer: {
      service: "huddles-pillbox-issuer-managed-burnin",
      entrypoint: "PillboxGrantIssuerEntrypoint",
    },
    currentness: {
      service: "huddles-projectors-managed-burnin",
      entrypoint: "PillboxAuthorizationCurrentnessEntrypoint",
    },
  },
  limits: { max_concurrent_executions: 1 },
};
const bootstrap = {
  ...bootstrapBase,
  authority_receipt: authorityReceipt(bootstrapBase),
};

const config = `
name = "pillbox-managed-burnin"

[vars]
PILLBOX_BOOTSTRAP_REQUIRED = "1"
PILLBOX_GRANT_KEY_ID = "${bootstrap.signing_key.key_id}"
PILLBOX_GRANT_PUBLIC_KEY = "${bootstrap.signing_key.public_key}"
PILLBOX_INSTALLATION_ID = "${bootstrap.installation.installation_id}"
PILLBOX_EXECUTION_REALM_ID = "${bootstrap.installation.execution_realm_id}"
PILLBOX_PROTOCOL_REVISION = "${bootstrap.installation.protocol_revision}"
PILLBOX_ORGANIZATION_ID = "${bootstrap.installation.organization_id}"
PILLBOX_MANAGED_CONCURRENCY = "1"
MANAGED_EXECUTION_ENABLED = "0"
MANAGED_EXECUTION_EPOCH = "burnin-test-v1"
MANAGED_EXECUTION_LIMIT = "1"

[[d1_databases]]
binding = "EXECUTION_DB"
database_name = "pillbox-managed-burnin-db"
database_id = "0123456789abcdef0123456789abcdef"
migrations_dir = "migrations"

[[r2_buckets]]
binding = "EXECUTION_EVIDENCE"
bucket_name = "pillbox-managed-burnin-evidence"

[[analytics_engine_datasets]]
binding = "RUN_COSTS"
dataset = "pillbox_managed_burnin_costs"

[[durable_objects.bindings]]
name = "Sandbox"
class_name = "Sandbox"

[[containers]]
class_name = "Sandbox"
image = "./Dockerfile"
instance_type = "standard-2"
max_instances = 1

[[migrations]]
tag = "v1"
new_sqlite_classes = ["Sandbox"]

[[services]]
binding = "PillboxAuthorizationCurrentness"
service = "huddles-projectors-managed-burnin"
entrypoint = "PillboxAuthorizationCurrentnessEntrypoint"
`;

const metadata = { secrets: [{ name: "MANAGED_CAPABILITY_SECRET", type: "secret_text" }] };
const remoteD1Result = [
  {
    results: [
      { name: "0001_execution.sql" },
      { name: "0002_managed_execution_allowance.sql" },
      { name: "0003_workspace_finalize.sql" },
    ],
    success: true,
  },
  {
    results: [{
    deployment_epoch: "burnin-test-v1",
    execution_limit: 1,
    reserved_executions: 0,
    }],
    success: true,
  },
];
const queryRemoteD1 = async () => JSON.stringify(remoteD1Result);
const validateBurninBootstrap = (input) =>
  validateBurninBootstrapRaw({ queryRemoteD1, ...input });

test("exact Huddles tuple and resolved Wrangler metadata pass without secret output", async () => {
  const result = await validateBurninBootstrap({ bootstrap, wrangler: config, metadata });

  assert.equal(result.status, "valid");
  assert.equal(result.signer.fingerprint, fingerprint);
  assert.equal(result.max_concurrent_executions, 1);
  assert.equal(result.capability_secret, "installed");
  assert.deepEqual(result.d1_receipt.allowance, remoteD1Result[1].results[0]);
  assert.equal(result.d1_receipt.source, "validator-owned-wrangler-d1-execute-remote");
  assert.match(result.d1_receipt.raw_output_sha256, /^sha256:[0-9a-f]{64}$/);
  assert.doesNotMatch(JSON.stringify(result), /secret_text/);
});

test("validator-owned raw remote D1 output must prove exact migrations and allowance", async () => {
  await assert.rejects(
    validateBurninBootstrapRaw({ bootstrap, wrangler: config, metadata }),
    /validator-owned remote D1 query runner is required/,
  );
  for (const mutate of [
    (result) => result[0].results.pop(),
    (result) => { result[1].results[0].deployment_epoch = "other-epoch"; },
    (result) => { result[1].results[0].execution_limit = 2; },
    (result) => { result[1].results[0].reserved_executions = 1; },
    (result) => { result[1].success = false; },
  ]) {
    const invalid = structuredClone(remoteD1Result);
    mutate(invalid);
    await assert.rejects(
      validateBurninBootstrap({
        bootstrap,
        wrangler: config,
        metadata,
        queryRemoteD1: async (request) => {
          assert.deepEqual(request, {
            database_name: "pillbox-managed-burnin-db",
            database_id: "0123456789abcdef0123456789abcdef",
            query: D1_BOOTSTRAP_QUERY,
          });
          return JSON.stringify(invalid);
        },
      }),
      /remote D1/,
    );
  }
});

test("fabricated normalized D1 receipts are not accepted as query output", async () => {
  await assert.rejects(
    validateBurninBootstrap({
      bootstrap,
      wrangler: config,
      metadata,
      queryRemoteD1: async () => JSON.stringify({
        schema_version: "pillbox.managed-d1-bootstrap-receipt/1",
        migrations: remoteD1Result[0].results.map(({ name }) => name),
        allowance: remoteD1Result[1].results[0],
      }),
    }),
    /exactly the migration and allowance query results/,
  );
});

test("missing capability-secret metadata fails closed", async () => {
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: config, metadata: { secrets: [] } }),
    /MANAGED_CAPABILITY_SECRET is not installed/,
  );
});

test("dry-run tuples and unverified applied authority receipts fail closed", async () => {
  await assert.rejects(
    validateBurninBootstrap({
      bootstrap: { ...bootstrap, mode: "dry-run" },
      wrangler: config,
      metadata,
    }),
    /mode must be apply/,
  );

  const unverified = structuredClone(bootstrap);
  unverified.authority_receipt.issuer_self_test.status = "configured";
  await assert.rejects(
    validateBurninBootstrap({ bootstrap: unverified, wrangler: config, metadata }),
    /issuer_self_test status must be verified/,
  );
});

test("authority receipt digest and exact database, issuer, and currentness proofs are required", async () => {
  const wrongDigest = structuredClone(bootstrap);
  wrongDigest.authority_receipt.receipt_sha256 = `sha256:${"0".repeat(64)}`;
  await assert.rejects(
    validateBurninBootstrap({ bootstrap: wrongDigest, wrangler: config, metadata }),
    /receipt_sha256 does not match/,
  );

  for (const [section, field] of [
    ["database", "policy_id"],
    ["issuer_self_test", "key_id"],
    ["currentness_probe", "installation_id"],
  ]) {
    const mismatched = structuredClone(bootstrap);
    mismatched.authority_receipt[section][field] = "other";
    mismatched.authority_receipt = authorityReceiptFromBody(mismatched.authority_receipt);
    await assert.rejects(
      validateBurninBootstrap({ bootstrap: mismatched, wrangler: config, metadata }),
      /does not match the bootstrap tuple/,
    );
  }
});

test("mismatched deployment pins and currentness service fail closed", async () => {
  const parsed = parseWranglerToml(config);
  parsed.vars.PILLBOX_ORGANIZATION_ID = "other-organization";
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: parsed, metadata }),
    /PILLBOX_ORGANIZATION_ID does not match/,
  );

  const wrongService = parseWranglerToml(config);
  wrongService.services[0].service = "other-currentness";
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: wrongService, metadata }),
    /currentness service does not match/,
  );
});

test("placeholder public keys, D1 IDs, and concurrency never validate", async () => {
  const placeholderKey = structuredClone(bootstrap);
  placeholderKey.signing_key.public_key =
    "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
  await assert.rejects(
    validateBurninBootstrap({
      bootstrap: placeholderKey,
      wrangler: {
        ...parseWranglerToml(config),
        vars: {
          ...parseWranglerToml(config).vars,
          PILLBOX_GRANT_PUBLIC_KEY: placeholderKey.signing_key.public_key,
        },
      },
      metadata,
    }),
    /public_key is invalid or a placeholder/,
  );

  const placeholderDatabase = parseWranglerToml(config);
  placeholderDatabase.d1_databases[0].database_id =
    "00000000-0000-4000-8000-000000000004";
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: placeholderDatabase, metadata }),
    /database_id is invalid or still a placeholder/,
  );

  const wrongConcurrency = parseWranglerToml(config);
  wrongConcurrency.containers[0].max_instances = 2;
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: wrongConcurrency, metadata }),
    /Sandbox container tuple does not match/,
  );
});

test("full topology requires isolated RUN_COSTS and only the vendor Sandbox tuple", async () => {
  const missingAnalytics = parseWranglerToml(config);
  missingAnalytics.analytics_engine_datasets = [];
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: missingAnalytics, metadata }),
    /isolated RUN_COSTS binding/,
  );

  const customDo = parseWranglerToml(config);
  customDo["durable_objects.bindings"].push({
    name: "SessionGateway",
    class_name: "SessionGateway",
  });
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: customDo, metadata }),
    /only the vendor Sandbox Durable Object binding/,
  );

  const customMigration = parseWranglerToml(config);
  customMigration.migrations[0].new_sqlite_classes.push("SessionGateway");
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: customMigration, metadata }),
    /only introduce the vendor Sandbox class/,
  );
});

function authorityReceipt(tuple) {
  return authorityReceiptFromBody({
    schema_version: "huddles.pillbox-burnin-authority-receipt/1",
    applied_at: "2026-09-01T00:00:00.000Z",
    database: {
      status: "verified",
      installation_id: tuple.installation.installation_id,
      execution_realm_id: tuple.installation.execution_realm_id,
      organization_id: tuple.installation.organization_id,
      workspace_id: tuple.installation.workspace_id,
      principal_id: tuple.installation.principal_id,
      policy_id: tuple.installation.policy_id,
      key_id: tuple.signing_key.key_id,
      public_key_fingerprint: tuple.signing_key.fingerprint,
    },
    issuer_self_test: {
      status: "verified",
      service: tuple.services.issuer.service,
      entrypoint: tuple.services.issuer.entrypoint,
      key_id: tuple.signing_key.key_id,
      public_key_fingerprint: tuple.signing_key.fingerprint,
    },
    currentness_probe: {
      status: "verified",
      service: tuple.services.currentness.service,
      entrypoint: tuple.services.currentness.entrypoint,
      installation_id: tuple.installation.installation_id,
      key_id: tuple.signing_key.key_id,
      public_key_fingerprint: tuple.signing_key.fingerprint,
      policy_id: tuple.installation.policy_id,
    },
  });
}

function authorityReceiptFromBody(receipt) {
  const { receipt_sha256: _ignored, ...body } = receipt;
  return {
    ...body,
    receipt_sha256: `sha256:${createHash("sha256").update(canonicalJson(body)).digest("hex")}`,
  };
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort((left, right) => left < right ? -1 : left > right ? 1 : 0)
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}
