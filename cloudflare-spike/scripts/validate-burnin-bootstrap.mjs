#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const BOOTSTRAP_VERSION = "huddles.pillbox-burnin-bootstrap/1";
const PROTOCOL_REVISION = "pillbox.huddles/1";
const REQUIRED_SECRET = "MANAGED_CAPABILITY_SECRET";
const PLACEHOLDER_D1_ID = "00000000-0000-4000-8000-000000000004";

export async function validateBurninBootstrap({ bootstrap, wrangler, metadata }) {
  const tuple = requireRecord(bootstrap, "Huddles bootstrap tuple");
  if (tuple.schema_version !== BOOTSTRAP_VERSION) {
    fail(`bootstrap schema_version must be ${BOOTSTRAP_VERSION}`);
  }
  const installation = requireRecord(tuple.installation, "bootstrap installation");
  const signingKey = requireRecord(tuple.signing_key, "bootstrap signing_key");
  const services = requireRecord(tuple.services, "bootstrap services");
  const currentness = requireRecord(services.currentness, "bootstrap currentness service");
  const limits = requireRecord(tuple.limits, "bootstrap limits");

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

  const sandbox = resolved.containers.find((candidate) => candidate.class_name === "Sandbox");
  if (!sandbox || sandbox.max_instances !== limits.max_concurrent_executions) {
    fail("Sandbox max_instances does not match bootstrap concurrency");
  }
  const database = resolved.d1_databases.find(
    (candidate) => candidate.binding === "EXECUTION_DB",
  );
  if (!database) fail("EXECUTION_DB binding is missing");
  requireConfiguredString(database.database_name, "EXECUTION_DB database_name");
  requireConfiguredString(database.database_id, "EXECUTION_DB database_id");
  if (!isProvisionedD1Identifier(database.database_id)) {
    fail("EXECUTION_DB database_id is invalid or still a placeholder");
  }
  const evidence = resolved.r2_buckets.find(
    (candidate) => candidate.binding === "EXECUTION_EVIDENCE",
  );
  if (!evidence) fail("EXECUTION_EVIDENCE binding is missing");
  requireConfiguredString(evidence.bucket_name, "EXECUTION_EVIDENCE bucket_name");

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
    resources: {
      d1_database_name: database.database_name,
      r2_bucket_name: evidence.bucket_name,
    },
  };
}

export function parseWranglerToml(source) {
  const output = {
    vars: {},
    services: [],
    containers: [],
    d1_databases: [],
    r2_buckets: [],
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
    vars: { ...parsed.vars, ...(resolvedMetadata.vars ?? {}) },
    services: resolvedMetadata.services ?? parsed.services,
    containers: resolvedMetadata.containers ?? parsed.containers,
    d1_databases: resolvedMetadata.d1_databases ?? parsed.d1_databases,
    r2_buckets: resolvedMetadata.r2_buckets ?? parsed.r2_buckets,
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
  if (/^[0-9]+$/.test(value)) return Number(value);
  if (value === "true") return true;
  if (value === "false") return false;
  return value;
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
