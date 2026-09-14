// Operator-side evidence; the immutable runtime envelope is never amended.
export function validateCostReceipt(value, expected) {
  const failures = [];
  const check = (condition, message) => { if (!condition) failures.push(`cost receipt: ${message}`); };
  const object = (value, keys, path) => {
    const valid = value !== null && typeof value === "object" && !Array.isArray(value);
    check(valid, `${path} must be an object`);
    const result = valid ? value : {};
    for (const key of keys) check(Object.hasOwn(result, key), `${path}.${key} is required`);
    for (const key of Object.keys(result)) check(keys.includes(key), `${path}.${key} is not allowed`);
    return result;
  };
  const number = (value, path, integral = true) => {
    check(typeof value === "number" && Number.isFinite(value) && value >= 0 &&
      (!integral || Number.isSafeInteger(value)), `${path} must be a non-negative ${integral ? "safe integer" : "finite number"}`);
  };
  const text = (value, path) => check(typeof value === "string" && value.trim().length > 0, `${path} must be nonempty`);
  const digest = (value, path) => check(typeof value === "string" && /^sha256:[0-9a-f]{64}$/.test(value), `${path} must be a SHA-256 digest`);
  const receipt = object(value, ["schema_version", "execution_identity", "artifact_ref", "cost_ref", "d1", "container", "sources"], "receipt");
  check(receipt.schema_version === 1, "schema_version must be 1");
  for (const [field, keys] of [
    ["execution_identity", ["invocation_id", "idempotency_key", "session_id", "request_hash", "execution_digest", "execution_policy_revision"]],
    ["artifact_ref", ["key", "media_type", "bytes", "sha256"]],
  ]) {
    const identity = object(receipt[field], keys, field);
    for (const key of keys) {
      check(identity[key] !== undefined && identity[key] === expected?.[field]?.[key], `${field}.${key} does not match the recorded execution`);
    }
  }
  digest(receipt.cost_ref, "cost_ref");
  check(receipt.cost_ref === expected?.cost_ref, "cost_ref does not match the immutable envelope");
  const d1 = object(receipt.d1, ["pre_seal", "terminal_commit", "execution_total"], "d1");
  const pairs = {};
  for (const scope of ["pre_seal", "terminal_commit", "execution_total"]) {
    const pair = object(d1[scope], ["rows_read", "rows_written"], `d1.${scope}`);
    for (const key of ["rows_read", "rows_written"]) number(pair[key], `d1.${scope}.${key}`);
    pairs[scope] = pair;
  }
  const infra = expected?.cost?.infrastructure ?? {};
  check(pairs.pre_seal.rows_read === infra.d1_rows_read, "pre-seal D1 read delta is unexplained");
  check(pairs.pre_seal.rows_written + 1 === infra.d1_rows_written, "pre-seal writes plus planned terminal write disagree with envelope");
  check(pairs.terminal_commit.rows_written === 1, "terminal commit must match the one planned write");
  for (const key of ["rows_read", "rows_written"]) {
    const sum = pairs.pre_seal[key] + pairs.terminal_commit[key];
    check(Number.isSafeInteger(sum) && sum === pairs.execution_total[key], `D1 ${key} total does not equal pre-seal plus commit`);
    check(pairs.execution_total[key] === expected?.observed?.d1?.[key], `observed D1 ${key} total does not match receipt`);
  }
  const container = object(receipt.container, ["instance_id", "profile", "execution_duration_ms", "cpu_seconds", "memory_byte_seconds", "disk_byte_seconds", "egress_bytes"], "container");
  text(container.instance_id, "container.instance_id");
  text(container.profile, "container.profile");
  number(container.execution_duration_ms, "container.execution_duration_ms");
  number(container.egress_bytes, "container.egress_bytes");
  for (const key of ["cpu_seconds", "memory_byte_seconds", "disk_byte_seconds"]) number(container[key], `container.${key}`, false);
  check(container.execution_duration_ms === infra.sandbox_duration_ms, "execution duration does not match envelope");
  check(container.profile === infra.sandbox_profile, "container profile does not match envelope");

  const sourceKeys = ["d1_pre_seal", "d1_terminal_commit", "d1_execution_total", "container_lifecycle"];
  const sources = object(receipt.sources, sourceKeys, "sources");
  const windows = {};
  const resources = {};
  for (const name of sourceKeys) {
    const source = object(sources[name], ["dataset", "resource_id", "capture_sha256", "from", "to"], `sources.${name}`);
    text(source.dataset, `sources.${name}.dataset`);
    text(source.resource_id, `sources.${name}.resource_id`);
    digest(source.capture_sha256, `sources.${name}.capture_sha256`);
    resources[name] = source.resource_id;
    const window = {};
    for (const key of ["from", "to"]) {
      const time = typeof source[key] === "string" ? Date.parse(source[key]) : NaN;
      check(Number.isFinite(time) && new Date(time).toISOString() === source[key], `sources.${name}.${key} must be canonical ISO UTC`);
      window[key] = time;
    }
    check(window.from < window.to, `sources.${name} requires from < to`);
    windows[name] = window;
  }
  const total = windows.d1_execution_total;
  for (const name of ["d1_pre_seal", "d1_terminal_commit"]) {
    check(resources[name] === resources.d1_execution_total, `${name} names another D1 database`);
    check(windows[name].from >= total.from && windows[name].to <= total.to, `${name} window must fit execution-total window`);
  }
  check(windows.d1_pre_seal.to <= windows.d1_terminal_commit.from, "pre-seal and commit windows overlap");
  check(resources.container_lifecycle === container.instance_id, "lifecycle source must name the exact container instance");
  check(windows.container_lifecycle.from <= total.from && windows.container_lifecycle.to >= total.to, "lifecycle window must contain execution-total window");
  return failures;
}
