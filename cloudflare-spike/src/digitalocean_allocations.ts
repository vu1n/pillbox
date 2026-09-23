import type { RelationalDatabase, RelationalUsage } from "./execution_store.js";

export interface DigitalOceanAllocation {
  readonly invocation_id: string;
  readonly name: string;
  readonly config_id: string;
  readonly provider_session_id: string | null;
  readonly state: "creating" | "ready" | "stopping" | "deleted";
}
export interface DigitalOceanAllocations {
  reserve(invocationId: string, name: string, configId: string): Promise<boolean>;
  get(invocationId: string): Promise<DigitalOceanAllocation | null>;
  attach(invocationId: string, id: string): Promise<boolean>;
  stop(invocationId: string, name: string): Promise<DigitalOceanAllocation>;
  deleted(invocationId: string): Promise<void>;
}

/** One bounded row per execution; no prompts, secrets, event streams or remote session authority. */
export class D1DigitalOceanAllocations implements DigitalOceanAllocations {
  private readonly db: RelationalDatabase;
  private readonly observe: (usage: RelationalUsage) => void;
  constructor(db: RelationalDatabase, observe: (usage: RelationalUsage) => void) {
    this.db = db;
    this.observe = observe;
  }
  private async query<T>(sql: string, values: readonly unknown[]): Promise<readonly T[]> {
    const result = await this.db
      .prepare(sql)
      .bind(...values)
      .all<T>();
    this.observe({
      rows_read: result.meta?.rows_read ?? 0,
      rows_written: result.meta?.rows_written ?? 0,
    });
    return result.results ?? [];
  }
  async reserve(invocationId: string, name: string, configId: string): Promise<boolean> {
    const rows = await this.query<DigitalOceanAllocation>(
      `INSERT INTO digitalocean_allocations (invocation_id, name, config_id, state)
       VALUES (?, ?, ?, 'creating') ON CONFLICT(invocation_id) DO NOTHING RETURNING *`,
      [invocationId, name, configId],
    );
    return rows.length === 1;
  }
  async get(invocationId: string): Promise<DigitalOceanAllocation | null> {
    const rows = await this.query<DigitalOceanAllocation>(
      "SELECT * FROM digitalocean_allocations WHERE invocation_id = ? LIMIT 1",
      [invocationId],
    );
    return rows[0] ?? null;
  }
  async attach(invocationId: string, id: string): Promise<boolean> {
    // Preserve a racing cancellation fence, but retain the newly returned identity for cleanup.
    const rows = await this.query<DigitalOceanAllocation>(
      `UPDATE digitalocean_allocations SET provider_session_id = ?,
       state = CASE WHEN state = 'creating' THEN 'ready' ELSE state END
       WHERE invocation_id = ? AND state != 'deleted' RETURNING *`,
      [id, invocationId],
    );
    return rows[0]?.state === "ready";
  }
  async stop(invocationId: string, name: string): Promise<DigitalOceanAllocation> {
    const rows = await this.query<DigitalOceanAllocation>(
      `INSERT INTO digitalocean_allocations (invocation_id, name, config_id, state)
       VALUES (?, ?, '', 'deleted') ON CONFLICT(invocation_id) DO UPDATE SET
       state = CASE WHEN state = 'deleted' THEN 'deleted' ELSE 'stopping' END RETURNING *`,
      [invocationId, name],
    );
    if (!rows[0]) throw new Error("DigitalOcean cleanup fence was not persisted");
    return rows[0];
  }
  async deleted(invocationId: string): Promise<void> {
    await this.query(
      "UPDATE digitalocean_allocations SET state = 'deleted' WHERE invocation_id = ? RETURNING invocation_id",
      [invocationId],
    );
  }
}
