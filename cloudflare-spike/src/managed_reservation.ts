import type { InvocationRequestHash } from "./codex_execution.js";
import type {
  RelationalDatabase,
  RelationalResult,
  RelationalStatement,
  RelationalUsage,
} from "./execution_store.js";
import type { ManagedExecutionOwner } from "./managed_ownership.js";

export const MAX_MANAGED_EXECUTION_LIMIT = 1_000;

export interface ManagedExecutionAllowance {
  readonly deployment_epoch: string;
  readonly execution_limit: number;
}

export interface ManagedExecutionAllowanceSnapshot
  extends ManagedExecutionAllowance {
  readonly reserved_executions: number;
}

export class ManagedExecutionAllowanceError extends Error {
  readonly code = "managed_disabled" as const;

  constructor() {
    super(
      "Pillbox managed execution allowance is exhausted or does not match the deployment configuration",
    );
    this.name = "ManagedExecutionAllowanceError";
  }
}

export class ManagedReservationAccessError extends Error {
  readonly code = "execution_not_found" as const;

  constructor() {
    super("managed execution reservation is unavailable");
    this.name = "ManagedReservationAccessError";
  }
}

export function parseManagedExecutionAllowance(
  deploymentEpoch: string | undefined,
  executionLimit: string | undefined,
): ManagedExecutionAllowance | null {
  if (
    deploymentEpoch === undefined ||
    !/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(deploymentEpoch) ||
    executionLimit === undefined ||
    !/^[1-9][0-9]*$/.test(executionLimit)
  ) {
    return null;
  }
  const parsedLimit = Number(executionLimit);
  if (
    !Number.isSafeInteger(parsedLimit) ||
    parsedLimit > MAX_MANAGED_EXECUTION_LIMIT
  ) {
    return null;
  }
  return {
    deployment_epoch: deploymentEpoch,
    execution_limit: parsedLimit,
  };
}

export type ManagedReservationSource =
  | "workspace_provision"
  | "direct_execution";
export type ManagedReservationStatus = "provisioning" | "ready" | "failed";

export interface ManagedExecutionReservation {
  readonly invocation_id: string;
  readonly session_id: string;
  readonly owner: ManagedExecutionOwner;
  readonly execution_request_hash: InvocationRequestHash;
  readonly source: ManagedReservationSource;
  readonly provision_request_digest: `sha256:${string}` | null;
  readonly status: ManagedReservationStatus;
  readonly error_code: string | null;
  readonly allowance_epoch: string;
  readonly allowance_limit: number;
  readonly created_at_ms: number;
  readonly updated_at_ms: number;
}

export type ManagedReservationClaim =
  | { readonly kind: "created"; readonly record: ManagedExecutionReservation }
  | { readonly kind: "reused"; readonly record: ManagedExecutionReservation }
  | { readonly kind: "conflict"; readonly record: ManagedExecutionReservation };

export interface ManagedReservationInput {
  readonly invocation_id: string;
  readonly session_id: string;
  readonly owner: ManagedExecutionOwner;
  readonly execution_request_hash: InvocationRequestHash;
  readonly source: ManagedReservationSource;
  readonly provision_request_digest?: `sha256:${string}`;
  readonly now_ms: number;
}

export interface ManagedExecutionReservationStore {
  claimProvision(
    input: ManagedReservationInput,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedReservationClaim>;
  claimExecution(
    input: Omit<ManagedReservationInput, "source" | "provision_request_digest">,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedReservationClaim>;
  getReady(
    invocationId: string,
    sessionId: string,
    owner: ManagedExecutionOwner,
    requestHash: InvocationRequestHash,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedExecutionReservation | null>;
  getSessionOwner(sessionId: string): Promise<ManagedExecutionOwner | null>;
  markReady(input: {
    readonly invocation_id: string;
    readonly owner: ManagedExecutionOwner;
    readonly now_ms: number;
  }): Promise<boolean>;
  markFailed(input: {
    readonly invocation_id: string;
    readonly owner: ManagedExecutionOwner;
    readonly error_code: string;
    readonly now_ms: number;
  }): Promise<boolean>;
  getAllowance(
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedExecutionAllowanceSnapshot | null>;
}

interface ReservationRow {
  invocation_id: string;
  session_id: string;
  owner_domain: ManagedExecutionOwner["domain"];
  owner_digest: `sha256:${string}`;
  execution_request_hash: InvocationRequestHash;
  source: ManagedReservationSource;
  provision_request_digest: `sha256:${string}` | null;
  status: ManagedReservationStatus;
  error_code: string | null;
  allowance_epoch: string;
  allowance_limit: number;
  created_at_ms: number;
  updated_at_ms: number;
}

type UsageObserver = (usage: RelationalUsage) => void;

const SELECT_COLUMNS = `
  invocation_id, session_id, owner_domain, owner_digest,
  execution_request_hash, source, provision_request_digest, status,
  error_code, allowance_epoch, allowance_limit, created_at_ms, updated_at_ms
`;

export class D1ManagedExecutionReservationStore
  implements ManagedExecutionReservationStore
{
  private readonly database: RelationalDatabase;
  private readonly observeUsage: UsageObserver;

  constructor(
    database: RelationalDatabase,
    observeUsage: UsageObserver = () => {},
  ) {
    this.database = database;
    this.observeUsage = observeUsage;
  }

  claimProvision(
    input: ManagedReservationInput,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedReservationClaim> {
    if (input.source !== "workspace_provision" || !input.provision_request_digest) {
      throw new Error("workspace provision reservation identity is incomplete");
    }
    return this.claim(input, allowance, "provisioning");
  }

  async claimExecution(
    input: Omit<ManagedReservationInput, "source" | "provision_request_digest">,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedReservationClaim> {
    const existing = await this.getOwned(input.invocation_id, input.owner);
    if (existing !== null) {
      return {
        kind: executionReservationMatches(existing, input, allowance)
          ? "reused"
          : "conflict",
        record: existing,
      };
    }
    return this.claim(
      { ...input, source: "direct_execution" },
      allowance,
      "ready",
      true,
    );
  }

  async getReady(
    invocationId: string,
    sessionId: string,
    owner: ManagedExecutionOwner,
    requestHash: InvocationRequestHash,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedExecutionReservation | null> {
    const result = await this.database
      .prepare(
        `SELECT ${SELECT_COLUMNS} FROM managed_execution_reservation
         WHERE invocation_id = ? AND session_id = ?
           AND owner_domain = ? AND owner_digest = ?
           AND execution_request_hash = ? AND allowance_epoch = ?
           AND allowance_limit = ? AND status = 'ready'
         LIMIT 1`,
      )
      .bind(
        invocationId,
        sessionId,
        owner.domain,
        owner.digest,
        requestHash,
        allowance.deployment_epoch,
        allowance.execution_limit,
      )
      .all<ReservationRow>();
    this.observe(result);
    return oneReservation(result);
  }

  async getSessionOwner(sessionId: string): Promise<ManagedExecutionOwner | null> {
    const result = await this.database
      .prepare(
        `SELECT owner_domain, owner_digest FROM managed_session_owner
         WHERE session_id = ? LIMIT 1`,
      )
      .bind(sessionId)
      .all<{ owner_domain: ManagedExecutionOwner["domain"]; owner_digest: `sha256:${string}` }>();
    this.observe(result);
    const rows = result.results ?? [];
    if (rows.length > 1) throw new Error("indexed session owner query returned multiple rows");
    const row = rows[0];
    return row === undefined
      ? null
      : { domain: row.owner_domain, digest: row.owner_digest };
  }

  markReady(input: {
    readonly invocation_id: string;
    readonly owner: ManagedExecutionOwner;
    readonly now_ms: number;
  }): Promise<boolean> {
    return this.transition(input, "ready");
  }

  async markFailed(input: {
    readonly invocation_id: string;
    readonly owner: ManagedExecutionOwner;
    readonly error_code: string;
    readonly now_ms: number;
  }): Promise<boolean> {
    const result = await this.run(
      `UPDATE managed_execution_reservation
       SET status = 'failed', error_code = ?, updated_at_ms = ?
       WHERE invocation_id = ? AND owner_domain = ? AND owner_digest = ?
         AND source = 'workspace_provision' AND status = 'provisioning'`,
      [
        input.error_code,
        input.now_ms,
        input.invocation_id,
        input.owner.domain,
        input.owner.digest,
      ],
    );
    return (result.meta?.changes ?? 0) === 1;
  }

  async getAllowance(
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedExecutionAllowanceSnapshot | null> {
    const result = await this.database
      .prepare(
        `SELECT deployment_epoch, execution_limit, reserved_executions
         FROM managed_execution_allowance
         WHERE singleton = 1 AND deployment_epoch = ? AND execution_limit = ?
         LIMIT 1`,
      )
      .bind(allowance.deployment_epoch, allowance.execution_limit)
      .all<ManagedExecutionAllowanceSnapshot>();
    this.observe(result);
    const rows = result.results ?? [];
    if (rows.length > 1) throw new Error("indexed allowance query returned multiple rows");
    return rows[0] ?? null;
  }

  private async claim(
    input: ManagedReservationInput,
    allowance: ManagedExecutionAllowance,
    status: "provisioning" | "ready",
    knownAbsent = false,
  ): Promise<ManagedReservationClaim> {
    if (!knownAbsent) {
      const existing = await this.getOwned(input.invocation_id, input.owner);
      if (existing !== null) {
        return {
          kind: reservationMatches(existing, input, allowance) ? "reused" : "conflict",
          record: existing,
        };
      }
    }
    let inserted: RelationalResult;
    try {
      inserted = await this.run(
        `INSERT OR IGNORE INTO managed_execution_reservation (
           invocation_id, session_id, owner_domain, owner_digest,
           execution_request_hash, source, provision_request_digest, status,
           allowance_epoch, allowance_limit, created_at_ms, updated_at_ms
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        [
          input.invocation_id,
          input.session_id,
          input.owner.domain,
          input.owner.digest,
          input.execution_request_hash,
          input.source,
          input.provision_request_digest ?? null,
          status,
          allowance.deployment_epoch,
          allowance.execution_limit,
          input.now_ms,
          input.now_ms,
        ],
      );
    } catch (cause) {
      const detail = String(cause);
      if (detail.includes("managed_execution_allowance_unavailable")) {
        throw new ManagedExecutionAllowanceError();
      }
      if (detail.includes("managed_session_owner_mismatch")) {
        throw new ManagedReservationAccessError();
      }
      throw cause;
    }
    if ((inserted.meta?.changes ?? 0) > 0) {
      return {
        kind: "created",
        record: reservationFromInput(input, allowance, status),
      };
    }
    const winner = await this.getOwned(input.invocation_id, input.owner);
    if (winner === null) throw new ManagedReservationAccessError();
    return {
      kind: reservationMatches(winner, input, allowance) ? "reused" : "conflict",
      record: winner,
    };
  }

  private async getOwned(
    invocationId: string,
    owner: ManagedExecutionOwner,
  ): Promise<ManagedExecutionReservation | null> {
    const result = await this.database
      .prepare(
        `SELECT ${SELECT_COLUMNS} FROM managed_execution_reservation
         WHERE invocation_id = ? AND owner_domain = ? AND owner_digest = ? LIMIT 1`,
      )
      .bind(invocationId, owner.domain, owner.digest)
      .all<ReservationRow>();
    this.observe(result);
    return oneReservation(result);
  }

  private async transition(
    input: {
      readonly invocation_id: string;
      readonly owner: ManagedExecutionOwner;
      readonly now_ms: number;
    },
    status: "ready",
  ): Promise<boolean> {
    const result = await this.run(
      `UPDATE managed_execution_reservation
       SET status = ?, updated_at_ms = ?
       WHERE invocation_id = ? AND owner_domain = ? AND owner_digest = ?
         AND source = 'workspace_provision' AND status = 'provisioning'`,
      [
        status,
        input.now_ms,
        input.invocation_id,
        input.owner.domain,
        input.owner.digest,
      ],
    );
    return (result.meta?.changes ?? 0) === 1;
  }

  private async run(
    sql: string,
    values: readonly unknown[],
  ): Promise<RelationalResult> {
    const result = await this.database.prepare(sql).bind(...values).run();
    this.observe(result);
    return result;
  }

  private observe(result: RelationalResult): void {
    this.observeUsage({
      rows_read: result.meta?.rows_read ?? 0,
      rows_written: result.meta?.rows_written ?? 0,
    });
  }
}

function executionReservationMatches(
  record: ManagedExecutionReservation,
  input: Omit<ManagedReservationInput, "source" | "provision_request_digest">,
  allowance: ManagedExecutionAllowance,
): boolean {
  return (
    record.invocation_id === input.invocation_id &&
    record.session_id === input.session_id &&
    record.owner.domain === input.owner.domain &&
    record.owner.digest === input.owner.digest &&
    record.execution_request_hash === input.execution_request_hash &&
    record.allowance_epoch === allowance.deployment_epoch &&
    record.allowance_limit === allowance.execution_limit
  );
}

function oneReservation(
  result: RelationalResult<ReservationRow>,
): ManagedExecutionReservation | null {
  const rows = result.results ?? [];
  if (rows.length > 1) throw new Error("indexed reservation query returned multiple rows");
  const row = rows[0];
  return row === undefined
    ? null
    : {
        invocation_id: row.invocation_id,
        session_id: row.session_id,
        owner: { domain: row.owner_domain, digest: row.owner_digest },
        execution_request_hash: row.execution_request_hash,
        source: row.source,
        provision_request_digest: row.provision_request_digest,
        status: row.status,
        error_code: row.error_code,
        allowance_epoch: row.allowance_epoch,
        allowance_limit: row.allowance_limit,
        created_at_ms: row.created_at_ms,
        updated_at_ms: row.updated_at_ms,
      };
}

function reservationFromInput(
  input: ManagedReservationInput,
  allowance: ManagedExecutionAllowance,
  status: "provisioning" | "ready",
): ManagedExecutionReservation {
  return {
    invocation_id: input.invocation_id,
    session_id: input.session_id,
    owner: input.owner,
    execution_request_hash: input.execution_request_hash,
    source: input.source,
    provision_request_digest: input.provision_request_digest ?? null,
    status,
    error_code: null,
    allowance_epoch: allowance.deployment_epoch,
    allowance_limit: allowance.execution_limit,
    created_at_ms: input.now_ms,
    updated_at_ms: input.now_ms,
  };
}

function reservationMatches(
  record: ManagedExecutionReservation,
  input: ManagedReservationInput,
  allowance: ManagedExecutionAllowance,
): boolean {
  return (
    record.invocation_id === input.invocation_id &&
    record.session_id === input.session_id &&
    record.owner.domain === input.owner.domain &&
    record.owner.digest === input.owner.digest &&
    record.execution_request_hash === input.execution_request_hash &&
    record.source === input.source &&
    record.provision_request_digest === (input.provision_request_digest ?? null) &&
    record.allowance_epoch === allowance.deployment_epoch &&
    record.allowance_limit === allowance.execution_limit
  );
}
