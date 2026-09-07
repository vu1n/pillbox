import assert from "node:assert/strict";
import { test } from "node:test";
import {
  MAX_MANAGED_EXECUTION_LIMIT,
  parseManagedExecutionAllowance,
} from "./src/managed_reservation.ts";

test("allowance config accepts only a bounded explicit epoch and limit", () => {
  assert.deepEqual(
    parseManagedExecutionAllowance("burnin-2026-09-01-a", "3"),
    { deployment_epoch: "burnin-2026-09-01-a", execution_limit: 3 },
  );
  for (const [epoch, limit] of [
    [undefined, "3"],
    ["", "3"],
    [" leading-space", "3"],
    ["epoch", undefined],
    ["epoch", "0"],
    ["epoch", "01"],
    ["epoch", "1.5"],
    ["epoch", String(MAX_MANAGED_EXECUTION_LIMIT + 1)],
  ] as const) {
    assert.equal(parseManagedExecutionAllowance(epoch, limit), null);
  }
});
