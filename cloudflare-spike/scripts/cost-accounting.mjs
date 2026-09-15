// Operator evidence partitions; no provider requests or runtime state writes.
export function validateCostAccounting(value, receipt, report) {
  const failures = [];
  const check = (ok, message) => { if (!ok) failures.push(`cost accounting: ${message}`); };
  const object = (value, keys, path) => {
    const ok = value !== null && typeof value === "object" && !Array.isArray(value);
    check(ok, `${path} must be an object`);
    const result = ok ? value : {};
    for (const key of keys) check(Object.hasOwn(result, key), `${path}.${key} is required`);
    for (const key of Object.keys(result)) check(keys.includes(key), `${path}.${key} is not allowed`);
    return result;
  };
  const text = (value, path) => check(typeof value === "string" && value.trim().length > 0 && value.length <= 1024, `${path} must be bounded nonempty text`);
  const number = (value, path, fractional = false) => check(typeof value === "number" && Number.isFinite(value) && value >= 0 &&
    (fractional || Number.isSafeInteger(value)), `${path} must be a non-negative ${fractional ? "finite number" : "safe integer"}`);
  const source = (value, path, r2 = false) => {
    const source = object(value, ["dataset", "resource_id", "capture_sha256", "from", "to", "record_ids", ...(r2 ? ["key_prefix"] : [])], path);
    for (const key of ["dataset", "resource_id"]) text(source[key], `${path}.${key}`);
    check(typeof source.capture_sha256 === "string" && /^sha256:[0-9a-f]{64}$/.test(source.capture_sha256), `${path}.capture_sha256 must be a SHA-256 digest`);
    const times = {};
    for (const key of ["from", "to"]) {
      times[key] = typeof source[key] === "string" ? Date.parse(source[key]) : NaN;
      check(Number.isFinite(times[key]) && new Date(times[key]).toISOString() === source[key], `${path}.${key} must be canonical ISO UTC`);
    }
    check(times.from < times.to, `${path} requires from < to`);
    const ids = Array.isArray(source.record_ids) ? source.record_ids : [];
    check(ids.length > 0 && ids.length <= 512, `${path}.record_ids requires 1–512 records`);
    for (const id of ids) text(id, `${path}.record_ids entry`);
    check(new Set(ids).size === ids.length, `${path} repeats a record`);
    if (r2) check(typeof source.key_prefix === "string" && source.key_prefix.length <= 1024, `${path}.key_prefix must be bounded text`);
    return { ...source, ...times, record_ids: ids };
  };
  const measurement = (value, keys, path, r2 = false) => {
    const record = object(value, ["units", "source"], path);
    const units = object(record.units, keys, `${path}.units`);
    for (const key of keys) number(units[key], `${path}.${key}`, key === "duration_gb_seconds");
    return { units, source: source(record.source, `${path}.source`, r2) };
  };
  const accounting = object(value, ["snapshot_prefix", "d1", "r2", "workers", "vendor_do", "vendor_retention"], "accounting");
  const specs = {
    d1: { parts: ["execution", "retry_status", "finalize", "operator_inspection"], keys: ["rows_read", "rows_written"] },
    r2: { parts: ["artifact", "snapshot_restore", "snapshot_finalize", "verification", "bucket_probes"], keys: ["reads", "writes", "lists", "heads", "deletes", "bytes_read", "bytes_written"] },
    workers: { parts: ["runtime", "issuer", "currentness", "operator"], keys: ["requests", "cpu_time_us"] },
    vendor_do: { parts: ["execute", "cleanup_idle"], keys: ["sql_rows_read", "sql_rows_written", "duration_gb_seconds"] },
  };
  const groups = {};
  const executionWindow = receipt?.sources?.d1_execution_total;
  for (const [name, { parts, keys }] of Object.entries(specs)) {
    const input = object(accounting[name], [...parts, "total"], name);
    const group = {};
    for (const part of [...parts, "total"]) group[part] = measurement(input[part], keys, `${name}.${part}`, name === "r2");
    const total = group.total;
    check(total.source.from <= Date.parse(executionWindow?.from) && total.source.to >= Date.parse(executionWindow?.to), `${name} total must contain the execution window`);
    const seen = new Set();
    for (const part of [...parts, "total"]) {
      const s = group[part].source;
      check(s.dataset === total.source.dataset, `${name}.${part} uses another dataset`);
      check(s.from >= total.source.from && s.to <= total.source.to, `${name}.${part} window lies outside total`);
      if (name !== "workers") check(s.resource_id === total.source.resource_id, `${name}.${part} names another resource`);
      for (const id of s.record_ids) {
        // Capture hashes may differ for the same provider observation. They must
        // not let one request/aggregate be counted under two partition labels.
        const identity = JSON.stringify([s.resource_id, id]);
        check(!seen.has(identity), `${name}.${part} reuses a partition or total record`);
        seen.add(identity);
      }
    }
    for (const key of keys) {
      const sum = parts.reduce((sum, part) => sum + group[part].units[key], 0);
      const actual = total.units[key];
      if (key === "duration_gb_seconds") {
        check(Number.isFinite(sum) && Math.abs(sum - actual) <= 1e-9, `${name}.${key} has an unexplained delta`);
      } else {
        check(Number.isSafeInteger(sum) && sum === actual, `${name}.${key} has an unexplained delta`);
      }
    }
    groups[name] = group;
  }
  check(report?.schema_version === 3, "report-v3 context is required");
  const capture = report?.capture;
  const runs = capture?.run_cost_envelopes;
  check(Array.isArray(runs) && runs.length === 1, "exactly one captured run is required");
  const run = Array.isArray(runs) ? runs[0] : undefined;
  const equalUnits = (actual, expected, keys, label) => {
    for (const key of keys) check(actual[key] !== undefined && actual[key] === expected?.[key], `${label}.${key} disagrees with runtime evidence`);
  };
  const { d1, r2, workers, vendor_do: vendor } = groups;
  check(d1.total.source.resource_id === receipt?.sources?.d1_execution_total?.resource_id, "D1 accounting names another execution database");
  check(d1.execution.source.from === Date.parse(executionWindow?.from) && d1.execution.source.to === Date.parse(executionWindow?.to), "D1 execution scope must use the receipt execution window");
  equalUnits(d1.execution.units, receipt?.d1?.execution_total, specs.d1.keys, "D1 execution");
  for (const key of specs.d1.keys) {
    check(d1.retry_status.units[key] + d1.finalize.units[key] === capture?.read_only?.d1?.[key], `D1 retry/status + finalize ${key} disagrees with report`);
  }
  check(d1.retry_status.units.rows_written === 0, "retry/status cannot write D1 rows");
  check(d1.operator_inspection.units.rows_written === 0, "operator inspection cannot write D1 rows");

  text(accounting.snapshot_prefix, "snapshot_prefix");
  const prefix = accounting.snapshot_prefix;
  check(typeof prefix === "string" && prefix.endsWith("/") && !prefix.startsWith("executions/") && !"executions/".startsWith(prefix), "snapshot_prefix must be disjoint from executions/");
  check(typeof receipt?.artifact_ref?.key === "string" && typeof prefix === "string" && !receipt.artifact_ref.key.startsWith(prefix), "snapshot_prefix overlaps the recorded artifact key");
  equalUnits(r2.artifact.units, capture?.totals?.r2, ["reads", "writes", "bytes_read", "bytes_written"], "R2 artifact");
  check(r2.artifact.source.key_prefix === receipt?.artifact_ref?.key, "artifact selector must name the exact artifact key");
  for (const key of ["lists", "heads", "deletes"]) check(r2.artifact.units[key] === 0, `artifact ${key} must be zero`);
  for (const part of ["snapshot_restore", "snapshot_finalize", "verification"]) {
    check(r2[part].source.key_prefix === prefix, `${part} must select the snapshot prefix`);
    check(r2[part].units.deletes === 0, `${part} cannot hide destructive cleanup`);
  }
  for (const part of ["snapshot_restore", "verification"]) {
    check(r2[part].units.writes === 0 && r2[part].units.bytes_written === 0, `${part} must be read-only`);
  }
  for (const key of ["reads", "writes", "deletes", "bytes_read", "bytes_written"]) check(r2.bucket_probes.units[key] === 0, `bucket probes cannot hide ${key}`);
  for (const part of ["bucket_probes", "total"]) check(r2[part].source.key_prefix === "", `${part} must select the whole bucket`);

  const scripts = specs.workers.parts.map(part => workers[part].source.resource_id);
  check(new Set(scripts).size === scripts.length, "Worker scopes must name distinct scripts");
  check(!scripts.includes(workers.total.source.resource_id), "Worker total must name the account, not a script");
  check(workers.runtime.source.resource_id === report?.deployment?.worker_name, "runtime Worker scope names another script");
  check(workers.runtime.units.requests === capture?.totals?.worker?.requests, "runtime Worker requests disagree with report");
  check(vendor.total.source.resource_id === receipt?.container?.instance_id, "vendor lifecycle names another container/DO instance");
  for (const [part, expected] of [["execute", run?.observed?.vendor_sandbox_do], ["cleanup_idle", capture?.read_only?.vendor_sandbox_do], ["total", capture?.totals?.vendor_sandbox_do]]) {
    check(vendor[part].units.sql_rows_read === expected?.storage_reads, `vendor ${part} SQL reads disagree with report`);
    check(vendor[part].units.sql_rows_written === expected?.storage_writes, `vendor ${part} SQL writes disagree with report`);
  }
  const retention = object(accounting.vendor_retention, ["before", "after"], "vendor_retention");
  const before = measurement(retention.before, ["stored_bytes"], "vendor_retention.before");
  const after = measurement(retention.after, ["stored_bytes"], "vendor_retention.after");
  check(before.source.resource_id === after.source.resource_id, "retention observations name different namespaces");
  check(before.source.dataset === after.source.dataset, "retention observations use different datasets");
  check(!before.source.record_ids.some(id => after.source.record_ids.includes(id)), "retention observations reuse a provider record");
  check(before.source.to <= after.source.from, "retention before/after windows overlap or are reversed");
  check(before.source.to <= Date.parse(executionWindow?.from) && after.source.from >= vendor.total.source.to, "retention observations must bracket the measured lifecycle");
  check(before.units.stored_bytes === capture?.totals?.vendor_sandbox_do?.stored_bytes_before, "retention before disagrees with namespace total");
  check(after.units.stored_bytes === capture?.totals?.vendor_sandbox_do?.stored_bytes_after, "retention after disagrees with namespace total");
  return failures;
}
