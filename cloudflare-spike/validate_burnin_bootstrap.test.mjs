import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import {
  parseWranglerToml,
  validateBurninBootstrap,
} from "./scripts/validate-burnin-bootstrap.mjs";

const publicKey = "ed25519:SKaA_nE69844nq00przjwmcPcg5iRY1hL4fnfgipY94";
const fingerprint = `sha256:${createHash("sha256")
  .update(Buffer.from(publicKey.slice("ed25519:".length), "base64url"))
  .digest("hex")}`;

const bootstrap = {
  schema_version: "huddles.pillbox-burnin-bootstrap/1",
  mode: "dry-run",
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

[[r2_buckets]]
binding = "EXECUTION_EVIDENCE"
bucket_name = "pillbox-managed-burnin-evidence"

[[containers]]
class_name = "Sandbox"
max_instances = 1

[[services]]
binding = "PillboxAuthorizationCurrentness"
service = "huddles-projectors-managed-burnin"
entrypoint = "PillboxAuthorizationCurrentnessEntrypoint"
`;

const metadata = { secrets: [{ name: "MANAGED_CAPABILITY_SECRET", type: "secret_text" }] };

test("exact Huddles tuple and resolved Wrangler metadata pass without secret output", async () => {
  const result = await validateBurninBootstrap({ bootstrap, wrangler: config, metadata });

  assert.equal(result.status, "valid");
  assert.equal(result.signer.fingerprint, fingerprint);
  assert.equal(result.max_concurrent_executions, 1);
  assert.equal(result.capability_secret, "installed");
  assert.doesNotMatch(JSON.stringify(result), /secret_text/);
});

test("missing capability-secret metadata fails closed", async () => {
  await assert.rejects(
    validateBurninBootstrap({ bootstrap, wrangler: config, metadata: { secrets: [] } }),
    /MANAGED_CAPABILITY_SECRET is not installed/,
  );
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
    /max_instances does not match/,
  );
});
