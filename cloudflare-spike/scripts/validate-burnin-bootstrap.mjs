#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const BOOTSTRAP_VERSION = "huddles.pillbox-burnin-bootstrap/1";
const PROTOCOL_REVISION = "pillbox.huddles/1";
const REQUIRED_SECRET = "MANAGED_CAPABILITY_SECRET";
const PLACEHOLDER_D1_ID = "00000000-0000-4000-8000-000000000004";
const AUTHORITY_RECEIPT_VERSION = "huddles.pillbox-burnin-authority-receipt/1";
const ISOLATED_RESOURCES = Object.freeze({
  worker: "pillbox-managed-burnin",
  d1: "pillbox-managed-burnin-db",
  r2: "pillbox-managed-burnin-evidence",
  analytics: "pillbox_managed_burnin_costs",
});

export async function validateBurninBootstrap({ bootstrap, wrangler, metadata }) {
  const tuple = requireRecord(bootstrap, "Huddles bootstrap tuple");
  if (tuple.schema_version !== BOOTSTRAP_VERSION) {
    fail(`bootstrap schema_version must be ${BOOTSTRAP_VERSION}`);
  }
  const installation = requireRecord(tuple.installation, "bootstrap installation");
  const signingKey = requireRecord(tuple.signing_key, "bootstrap signing_key");
  const services = requireRecord(tuple.services, "bootstrap services");
  const currentness = requireRecord(services.currentness, "bootstrap currentness service");
  const issuer = requireRecord(services.issuer, "bootstrap issuer service");
  const limits = requireRecord(tuple.limits, "bootstrap limits");

  if (tuple.mode !== "apply") fail("bootstrap mode must be apply");
  const authorityReceipt = validateAuthorityReceipt(tuple, issuer, currentness);

  const resolved = resolvedWrangler(wrangler, metadata);
  const vars = resolved.vars;
  const expectedPins = new Map([
    ["PILLBOX_GRANT_KEY_ID", signingKey.key_id],
    ["PILLBOX_GRANT_PUBLIC_KEY", signingKey.public_key],
    ["PILLBOX_INSTALLATION_ID", installation.installation_id],
    ["PILLBOX_EXECUTION_REALM_ID", installation.execution_realm_id],
    ["PILLBOX_PROTOCOL_REVISION", installation.protocol_revision],
    ["PILLBOX_ORGANIZATION_ID", installation.organization_id],
    ["PILLBOX_MANAGED_CONCURRENCY", String(limits.max_concurrent_executions)],
  ]);
  if (vars.PILLBOX_BOOTSTRAP_REQUIRED !== "1") {
    fail("PILLBOX_BOOTSTRAP_REQUIRED must be exactly 1");
  }
  for (const [name, expected] of expectedPins) {
    requireConfiguredString(expected, `bootstrap ${name}`);
    requireConfiguredString(vars[name], `Wrangler ${name}`);
    if (vars[name] !== expected) fail(`${name} does not match the Huddles bootstrap tuple`);
  }
  if (installation.protocol_revision !== PROTOCOL_REVISION) {
    fail(`bootstrap protocol_revision must be ${PROTOCOL_REVISION}`);
  }
  if (limits.max_concurrent_executions !== 1) {
    fail("burn-in max_concurrent_executions must be exactly 1");
  }
  if (vars.MANAGED_EXECUTION_LIMIT !== "1") {
    fail("MANAGED_EXECUTION_LIMIT must match the one-run burn-in limit");
  }
  requireConfiguredString(vars.MANAGED_EXECUTION_EPOCH, "Wrangler MANAGED_EXECUTION_EPOCH");
  if (vars.MANAGED_EXECUTION_ENABLED !== "0") {
    fail("MANAGED_EXECUTION_ENABLED must remain 0 during bootstrap validation");
  }

  const publicKeyBytes = decodePublicKey(signingKey.public_key);
  const fingerprint = `sha256:${createHash("sha256").update(publicKeyBytes).digest("hex")}`;
  if (signingKey.fingerprint !== fingerprint) {
    fail("bootstrap signing_key fingerprint does not match its public key");
  }

  const binding = resolved.services.find(
    (candidate) => candidate.binding === "PillboxAuthorizationCurrentness",
  );
  if (!binding) fail("Wrangler currentness service binding is missing");
  for (const field of ["service", "entrypoint"]) {
    requireConfiguredString(currentness[field], `bootstrap currentness ${field}`);
    if (binding[field] !== currentness[field]) {
      fail(`currentness ${field} does not match the Huddles bootstrap tuple`);
    }
  }

  if (resolved.name !== ISOLATED_RESOURCES.worker) {
    fail(`Wrangler Worker name must be ${ISOLATED_RESOURCES.worker}`);
  }
  if (resolved.containers.length !== 1) {
    fail("Wrangler must contain only the vendor Sandbox container");
  }
  const sandbox = resolved.containers[0];
  if (
    sandbox.class_name !== "Sandbox" ||
    sandbox.image !== "./Dockerfile" ||
    sandbox.instance_type !== "standard-2" ||
    sandbox.max_instances !== limits.max_concurrent_executions
  ) {
    fail("Sandbox container tuple does not match the isolated burn-in profile");
  }
  if (resolved.d1_databases.length !== 1) {
    fail("Wrangler must contain only the isolated EXECUTION_DB binding");
  }
  const database = resolved.d1_databases[0];
  if (database.binding !== "EXECUTION_DB") fail("EXECUTION_DB binding is missing");
  requireConfiguredString(database.database_name, "EXECUTION_DB database_name");
  requireConfiguredString(database.database_id, "EXECUTION_DB database_id");
  if (database.migrations_dir !== "migrations") {
    fail("EXECUTION_DB migrations_dir must be migrations");
  }
  if (database.database_name !== ISOLATED_RESOURCES.d1) {
    fail("EXECUTION_DB must use the isolated burn-in database name");
  }
  if (!isProvisionedD1Identifier(database.database_id)) {
    fail("EXECUTION_DB database_id is invalid or still a placeholder");
  }
  if (resolved.r2_buckets.length !== 1) {
    fail("Wrangler must contain only the isolated EXECUTION_EVIDENCE binding");
  }
  const evidence = resolved.r2_buckets[0];
  if (evidence.binding !== "EXECUTION_EVIDENCE") {
    fail("EXECUTION_EVIDENCE binding is missing");
  }
  requireConfiguredString(evidence.bucket_name, "EXECUTION_EVIDENCE bucket_name");
  if (evidence.bucket_name !== ISOLATED_RESOURCES.r2) {
    fail("EXECUTION_EVIDENCE must use the isolated burn-in bucket name");
  }

  if (resolved.analytics_engine_datasets.length !== 1) {
    fail("Wrangler must contain exactly one isolated RUN_COSTS binding");
  }
  const analytics = resolved.analytics_engine_datasets[0];
  if (
    analytics.binding !== "RUN_COSTS" ||
    analytics.dataset !== ISOLATED_RESOURCES.analytics
  ) {
    fail("isolated RUN_COSTS binding does not match the burn-in dataset");
  }

  if (
    resolved.durable_object_bindings.length !== 1 ||
    resolved.durable_object_bindings[0].name !== "Sandbox" ||
    resolved.durable_object_bindings[0].class_name !== "Sandbox"
  ) {
    fail("Wrangler may contain only the vendor Sandbox Durable Object binding");
  }
  if (
    resolved.migrations.length !== 1 ||
    resolved.migrations[0].tag !== "v1" ||
    !Array.isArray(resolved.migrations[0].new_sqlite_classes) ||
    resolved.migrations[0].new_sqlite_classes.length !== 1 ||
    resolved.migrations[0].new_sqlite_classes[0] !== "Sandbox"
  ) {
    fail("Wrangler migration may only introduce the vendor Sandbox class");
  }
  if (resolved.services.length !== 1) {
    fail("Wrangler must contain only the checked currentness service binding");
  }

  if (!resolved.secretNames.has(REQUIRED_SECRET)) {
    fail(`${REQUIRED_SECRET} is not installed according to Wrangler secret metadata`);
  }

  return {
    status: "valid",
    schema_version: BOOTSTRAP_VERSION,
    signer: { key_id: signingKey.key_id, fingerprint },
    installation: {
      installation_id: installation.installation_id,
      execution_realm_id: installation.execution_realm_id,
      protocol_revision: installation.protocol_revision,
      organization_id: installation.organization_id,
    },
    currentness: {
      service: binding.service,
      entrypoint: binding.entrypoint,
    },
    max_concurrent_executions: limits.max_concurrent_executions,
    capability_secret: "installed",
    authority_receipt: {
      applied_at: authorityReceipt.applied_at,
      receipt_sha256: authorityReceipt.receipt_sha256,
      database: "verified",
      issuer_self_test: "verified",
      currentness_probe: "verified",
    },
    resources: {
      d1_database_name: database.database_name,
      r2_bucket_name: evidence.bucket_name,
      analytics_dataset: analytics.dataset,
    },
  };
}

export function parseWranglerToml(source) {
  const output = {
    name: undefined,
    vars: {},
    services: [],
    containers: [],
    d1_databases: [],
    r2_buckets: [],
    analytics_engine_datasets: [],
    "durable_objects.bindings": [],
    migrations: [],
  };
  let target = output;
  for (const original of source.split(/\r?\n/)) {
    const line = original.trim();
    if (line.length === 0 || line.startsWith("#")) continue;
    const arrayHeader = /^\[\[([A-Za-z0-9_.-]+)\]\]$/.exec(line);
    if (arrayHeader) {
      const name = arrayHeader[1];
      if (!Object.hasOwn(output, name) || !Array.isArray(output[name])) {
        target = {};
        continue;
      }
      target = {};
      output[name].push(target);
      continue;
    }
    const tableHeader = /^\[([A-Za-z0-9_.-]+)\]$/.exec(line);
    if (tableHeader) {
      target = tableHeader[1] === "vars" ? output.vars : {};
      continue;
    }
    const assignment = /^([A-Za-z0-9_.-]+)\s*=\s*(.+)$/.exec(line);
    if (!assignment) continue;
    target[assignment[1]] = parseTomlScalar(assignment[2]);
  }
  return output;
}

function resolvedWrangler(config, metadata) {
  const parsed = typeof config === "string" ? parseWranglerToml(config) : config;
  const resolvedMetadata = Array.isArray(metadata) ? { secrets: metadata } : metadata ?? {};
  const merged = {
    name: resolvedMetadata.name ?? parsed.name,
    vars: { ...parsed.vars, ...(resolvedMetadata.vars ?? {}) },
    services: resolvedMetadata.services ?? parsed.services,
    containers: resolvedMetadata.containers ?? parsed.containers,
    d1_databases: resolvedMetadata.d1_databases ?? parsed.d1_databases,
    r2_buckets: resolvedMetadata.r2_buckets ?? parsed.r2_buckets,
    analytics_engine_datasets:
      resolvedMetadata.analytics_engine_datasets ?? parsed.analytics_engine_datasets,
    durable_object_bindings:
      resolvedMetadata.durable_object_bindings ??
      parsed["durable_objects.bindings"],
    migrations: resolvedMetadata.migrations ?? parsed.migrations,
    secretNames: new Set(),
  };
  for (const secret of resolvedMetadata.secret_names ?? resolvedMetadata.secrets ?? []) {
    const name = typeof secret === "string" ? secret : secret?.name;
    if (typeof name === "string") merged.secretNames.add(name);
  }
  return merged;
}

function parseTomlScalar(value) {
  if (/^"(?:[^"\\]|\\.)*"$/.test(value)) return JSON.parse(value);
  if (/^\[.*\]$/.test(value)) {
    try {
      return JSON.parse(value);
    } catch {
      return value;
    }
  }
  if (/^[0-9]+$/.test(value)) return Number(value);
  if (value === "true") return true;
  if (value === "false") return false;
  return value;
}

function validateAuthorityReceipt(tuple, issuer, currentness) {
  const receipt = requireRecord(tuple.authority_receipt, "bootstrap authority_receipt");
  requireExactKeys(
    receipt,
    [
      "schema_version",
      "applied_at",
      "database",
      "issuer_self_test",
      "currentness_probe",
      "receipt_sha256",
    ],
    "bootstrap authority_receipt",
  );
  if (receipt.schema_version !== AUTHORITY_RECEIPT_VERSION) {
    fail(`authority_receipt schema_version must be ${AUTHORITY_RECEIPT_VERSION}`);
  }
  if (
    typeof receipt.applied_at !== "string" ||
    !Number.isFinite(Date.parse(receipt.applied_at)) ||
    new Date(receipt.applied_at).toISOString() !== receipt.applied_at
  ) {
    fail("authority_receipt applied_at must be an exact ISO-8601 instant");
  }
  const database = requireRecord(receipt.database, "authority_receipt database");
  const issuerTest = requireRecord(
    receipt.issuer_self_test,
    "authority_receipt issuer_self_test",
  );
  const currentnessProbe = requireRecord(
    receipt.currentness_probe,
    "authority_receipt currentness_probe",
  );
  requireVerified(database, "database");
  requireVerified(issuerTest, "issuer_self_test");
  requireVerified(currentnessProbe, "currentness_probe");

  const installation = tuple.installation;
  const key = tuple.signing_key;
  requireReceiptMatches(database, {
    installation_id: installation.installation_id,
    execution_realm_id: installation.execution_realm_id,
    organization_id: installation.organization_id,
    workspace_id: installation.workspace_id,
    principal_id: installation.principal_id,
    policy_id: installation.policy_id,
    key_id: key.key_id,
    public_key_fingerprint: key.fingerprint,
  }, "database");
  requireReceiptMatches(issuerTest, {
    service: issuer.service,
    entrypoint: issuer.entrypoint,
    key_id: key.key_id,
    public_key_fingerprint: key.fingerprint,
  }, "issuer_self_test");
  requireReceiptMatches(currentnessProbe, {
    service: currentness.service,
    entrypoint: currentness.entrypoint,
    installation_id: installation.installation_id,
    key_id: key.key_id,
    public_key_fingerprint: key.fingerprint,
    policy_id: installation.policy_id,
  }, "currentness_probe");

  const { receipt_sha256: claimedDigest, ...digestBody } = receipt;
  const expectedDigest = `sha256:${createHash("sha256")
    .update(canonicalJson(digestBody))
    .digest("hex")}`;
  if (claimedDigest !== expectedDigest) {
    fail("authority_receipt receipt_sha256 does not match its canonical body");
  }
  return receipt;
}

function requireVerified(value, label) {
  if (value.status !== "verified") {
    fail(`authority_receipt ${label} status must be verified`);
  }
}

function requireReceiptMatches(actual, expected, label) {
  requireExactKeys(actual, ["status", ...Object.keys(expected)], `authority_receipt ${label}`);
  for (const [field, expectedValue] of Object.entries(expected)) {
    requireConfiguredString(actual[field], `authority_receipt ${label}.${field}`);
    if (actual[field] !== expectedValue) {
      fail(`authority_receipt ${label}.${field} does not match the bootstrap tuple`);
    }
  }
}

function requireExactKeys(value, expected, label) {
  const actual = Object.keys(value);
  const expectedSet = new Set(expected);
  const unknown = actual.find((key) => !expectedSet.has(key));
  if (unknown !== undefined) fail(`${label}.${unknown} is not allowed`);
  const missing = expected.find((key) => !Object.hasOwn(value, key));
  if (missing !== undefined) fail(`${label}.${missing} is required`);
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") {
    if (typeof value === "number" && !Number.isFinite(value)) {
      fail("authority_receipt contains a non-finite number");
    }
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort((left, right) => left < right ? -1 : left > right ? 1 : 0)
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

function decodePublicKey(value) {
  requireConfiguredString(value, "bootstrap signing_key public_key");
  if (!/^ed25519:[A-Za-z0-9_-]{43}$/.test(value)) {
    fail("bootstrap signing_key public_key is not a raw Ed25519 key");
  }
  const bytes = Buffer.from(value.slice("ed25519:".length), "base64url");
  if (bytes.length !== 32 || new Set(bytes).size <= 1) {
    fail("bootstrap signing_key public_key is invalid or a placeholder");
  }
  return bytes;
}

function requireConfiguredString(value, label) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    /placeholder|replace|unconfigured/i.test(value) ||
    /^<.*>$/.test(value)
  ) {
    fail(`${label} is not configured`);
  }
}

function requireRecord(value, label) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    fail(`${label} must be an object`);
  }
  return value;
}

function isProvisionedD1Identifier(value) {
  if (value === PLACEHOLDER_D1_ID) return false;
  const compact = value.replaceAll("-", "");
  return /^[0-9a-f]{32}$/.test(compact) && new Set(compact).size >= 8;
}

function fail(message) {
  throw new Error(`burn-in bootstrap invalid: ${message}`);
}

function option(name) {
  const index = process.argv.indexOf(name);
  if (index < 0 || index + 1 >= process.argv.length) return undefined;
  return process.argv[index + 1];
}

async function main() {
  const bootstrapPath = option("--bootstrap");
  const configPath = option("--config");
  const metadataPath = option("--metadata");
  if (!bootstrapPath || !configPath || !metadataPath) {
    fail("usage: validate-burnin-bootstrap.mjs --bootstrap <json> --config <toml> --metadata <json>");
  }
  const [bootstrap, wrangler, metadata] = await Promise.all([
    readJson(bootstrapPath, "Huddles bootstrap tuple"),
    readFile(configPath, "utf8"),
    readJson(metadataPath, "Wrangler metadata"),
  ]);
  const result = await validateBurninBootstrap({ bootstrap, wrangler, metadata });
  console.log(JSON.stringify(result, null, 2));
}

async function readJson(path, label) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (cause) {
    fail(`${label} could not be read: ${cause instanceof Error ? cause.message : String(cause)}`);
  }
}

if (process.argv[1] && pathToFileURL(process.argv[1]).href === import.meta.url) {
  main().catch((cause) => {
    console.error(cause instanceof Error ? cause.message : String(cause));
    process.exitCode = 1;
  });
}
