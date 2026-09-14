import assert from "node:assert/strict";
import { test } from "node:test";
import {
  ManagedAdmissionError,
  managedAdmissionPolicy,
  requireManagedAdmission,
} from "./src/managed_admission.ts";

test("managed admission fails closed unless explicitly enabled", () => {
  for (const value of [undefined, "", "0", "true", " 1 "]) {
    const policy = managedAdmissionPolicy(value);
    assert.equal(policy.new_managed_executions, "disabled");
    assert.throws(
      () => requireManagedAdmission(policy),
      (cause) =>
        cause instanceof ManagedAdmissionError &&
        cause.code === "managed_disabled" &&
        cause.message === "Pillbox managed execution is disabled",
    );
  }
});

test("managed admission accepts the explicit enable value", () => {
  const policy = managedAdmissionPolicy("1");
  assert.equal(policy.new_managed_executions, "enabled");
  assert.doesNotThrow(() => requireManagedAdmission(policy));
});
