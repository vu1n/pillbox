export interface ManagedAdmissionPolicy {
  readonly new_managed_executions: "enabled" | "disabled";
}

export class ManagedAdmissionError extends Error {
  readonly code = "managed_disabled" as const;

  constructor() {
    super("Pillbox managed execution is disabled");
    this.name = "ManagedAdmissionError";
  }
}

/** Fail closed unless the deployment explicitly enables new managed work. */
export function managedAdmissionPolicy(
  enabled: string | undefined,
): ManagedAdmissionPolicy {
  return {
    new_managed_executions: enabled === "1" ? "enabled" : "disabled",
  };
}

/** Guard every entrypoint before it reaches billed persistence or runtime state. */
export function requireManagedAdmission(
  policy: ManagedAdmissionPolicy,
): void {
  if (policy.new_managed_executions !== "enabled") {
    throw new ManagedAdmissionError();
  }
}
