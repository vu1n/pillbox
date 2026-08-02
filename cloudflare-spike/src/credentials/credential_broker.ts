import { DurableObject } from "cloudflare:workers";
import type { Env } from "../worker.js";
import { decryptOAuthMaterial, encryptOAuthMaterial } from "./crypto.js";
import {
  CredentialBrokerError,
  isCurrentRefreshGeneration,
  type CredentialLease,
  type CredentialLeaseRequest,
  type OAuthMaterial,
} from "./contract.js";

type StoredRow = { binding_id: string; secret_ref: string; purpose: string; execution_realm_id: string; provider_id: string; ciphertext: string; status: string; generation: number; pending_generation: number | null; provider_host: string; expires_at: number };

/** One Durable Object is the sole refresh writer for one installation profile. */
export class CredentialBroker extends DurableObject<Env> {
  private initialized = false;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
  }

  private schema(): void {
    if (this.initialized) return;
    this.ctx.storage.sql.exec(`CREATE TABLE IF NOT EXISTS credential(
      binding_id TEXT PRIMARY KEY,
      secret_ref TEXT NOT NULL,
      purpose TEXT NOT NULL,
      execution_realm_id TEXT NOT NULL,
      provider_id TEXT NOT NULL,
      ciphertext TEXT NOT NULL,
      status TEXT NOT NULL CHECK(status IN ('active','revoked','reauth_required')),
      generation INTEGER NOT NULL,
      pending_generation INTEGER,
      provider_host TEXT NOT NULL,
      expires_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    )`);
    const columns = new Set((this.ctx.storage.sql.exec("PRAGMA table_info(credential)").toArray() as Array<{ name: string }>).map((column) => column.name));
    if (!columns.has("provider_id")) this.ctx.storage.sql.exec("ALTER TABLE credential ADD COLUMN provider_id TEXT NOT NULL DEFAULT ''");
    this.initialized = true;
  }

  async putOAuthMaterial(input: { readonly binding_id: string; readonly secret_ref: string; readonly purpose: string; readonly execution_realm_id: string; readonly provider_id: string; readonly provider_host: string; readonly access_token: string; readonly refresh_token?: string; readonly expires_at: number }): Promise<{ generation: number }> {
    this.schema();
    const bindingId = required(input.binding_id, "binding_id");
    const secretRef = required(input.secret_ref, "secret_ref");
    const purpose = required(input.purpose, "purpose");
    const realm = required(input.execution_realm_id, "execution_realm_id");
    const providerId = required(input.provider_id, "provider_id");
    const host = normalizedHost(input.provider_host);
    required(input.access_token, "access_token");
    if (!Number.isSafeInteger(input.expires_at) || input.expires_at <= 0) throw new CredentialBrokerError("invalid_request", "expires_at is invalid");
    const current = this.row(bindingId);
    const generation = (current?.generation ?? 0) + 1;
    const material: OAuthMaterial = { access_token: input.access_token, ...(input.refresh_token ? { refresh_token: input.refresh_token } : {}), expires_at: input.expires_at, provider_host: host, generation };
    const ciphertext = await this.encrypt(material, bindingId);
    this.ctx.storage.sql.exec(`INSERT INTO credential(binding_id,secret_ref,purpose,execution_realm_id,provider_id,ciphertext,status,generation,pending_generation,provider_host,expires_at,updated_at)
      VALUES(?,?,?,?,?,?,'active',?,NULL,?,?,?) ON CONFLICT(binding_id) DO UPDATE SET secret_ref=excluded.secret_ref,purpose=excluded.purpose,execution_realm_id=excluded.execution_realm_id,provider_id=excluded.provider_id,ciphertext=excluded.ciphertext,status='active',generation=excluded.generation,pending_generation=NULL,provider_host=excluded.provider_host,expires_at=excluded.expires_at,updated_at=excluded.updated_at`, bindingId, secretRef, purpose, realm, providerId, ciphertext, generation, host, input.expires_at, now());
    return { generation };
  }

  async completeRefresh(input: { readonly binding_id: string; readonly expected_generation: number; readonly provider_id: string; readonly access_token: string; readonly refresh_token?: string; readonly expires_at: number }): Promise<{ generation: number }> {
    this.schema();
    const bindingId = required(input.binding_id, "binding_id");
    const row = this.row(bindingId);
    if (!row || row.status !== "active" || row.generation !== input.expected_generation || row.pending_generation !== input.expected_generation + 1 || row.provider_id !== required(input.provider_id, "provider_id")) throw new CredentialBrokerError("refresh_in_progress", "credential refresh generation is no longer current");
    const material: OAuthMaterial = { access_token: required(input.access_token, "access_token"), ...(input.refresh_token ? { refresh_token: input.refresh_token } : {}), expires_at: input.expires_at, provider_host: row.provider_host, generation: row.generation + 1 };
    const ciphertext = await this.encrypt(material, bindingId);
    this.ctx.storage.sql.exec("UPDATE credential SET ciphertext=?,status='active',generation=?,pending_generation=NULL,expires_at=?,updated_at=? WHERE binding_id=? AND generation=? AND pending_generation=?", ciphertext, material.generation, material.expires_at, now(), bindingId, input.expected_generation, input.expected_generation + 1);
    const updated = this.row(bindingId);
    if (!updated || updated.generation !== material.generation || updated.pending_generation !== null) throw new CredentialBrokerError("refresh_in_progress", "credential refresh completion lost its generation race");
    return { generation: updated.generation };
  }

  async revoke(bindingId: string): Promise<void> {
    this.schema();
    this.ctx.storage.sql.exec("UPDATE credential SET status='revoked', pending_generation=NULL, updated_at=? WHERE binding_id=?", now(), required(bindingId, "binding_id"));
  }

  async lease(input: CredentialLeaseRequest): Promise<CredentialLease> {
    this.schema();
    const row = this.row(required(input.route.credential_binding_id, "credential_binding_id"));
    if (!row) throw new CredentialBrokerError("not_found", "credential binding is unavailable");
    if (row.status === "revoked") throw new CredentialBrokerError("revoked", "credential binding is revoked");
    if (row.status === "reauth_required") throw new CredentialBrokerError("reauth_required", "credential binding requires re-authentication");
    const host = normalizedHost(input.route.host);
    if (host !== row.provider_host || input.route.secret_ref !== row.secret_ref || input.route.purpose !== row.purpose || input.execution_realm_id !== row.execution_realm_id) throw new CredentialBrokerError("wrong_host", "credential lease binding is not authorized");
    const material = await this.decrypt(row.ciphertext, row.binding_id);
    const nowSeconds = Math.floor(Date.now() / 1000);
    if (material.expires_at <= nowSeconds + 30) throw new CredentialBrokerError("reauth_required", "credential lease is expired or too close to expiry");
    return { lease_id: `${input.invocation_id}:${row.generation}`, generation: row.generation, provider_host: row.provider_host, expires_at: Math.min(material.expires_at, nowSeconds + 60), access_token: material.access_token };
  }

  /** Begin refresh in the trusted OAuth adapter; no callback crosses DO RPC. */
  async beginRefresh(bindingIdInput: string): Promise<{ readonly material: OAuthMaterial; readonly secret_ref: string; readonly purpose: string; readonly execution_realm_id: string; readonly provider_id: string; readonly provider_host: string; readonly generation: number }> {
    this.schema();
    const bindingId = required(bindingIdInput, "binding_id");
    const row = this.row(bindingId);
    if (!row) throw new CredentialBrokerError("not_found", "credential binding is unavailable");
    if (row.status === "revoked") throw new CredentialBrokerError("revoked", "credential binding is revoked");
    if (row.status === "reauth_required") throw new CredentialBrokerError("reauth_required", "credential binding requires re-authentication");
    if (row.pending_generation !== null) throw new CredentialBrokerError("refresh_in_progress", "credential refresh is already pending");
    const pending = row.generation + 1;
    const material = await this.decrypt(row.ciphertext, row.binding_id);
    // Decryption is a local, pre-provider step. Claim the generation only
    // after it succeeds so a lost encryption key cannot strand this binding in
    // refresh_in_progress forever. The generation predicate closes the race
    // with an OAuth callback or a newer material write during decryption.
    const claimed = this.ctx.storage.transactionSync(() => {
      const current = this.row(bindingId);
      if (!current || current.generation !== row.generation || current.pending_generation !== null || current.status !== "active") return false;
      this.ctx.storage.sql.exec("UPDATE credential SET pending_generation=?, updated_at=? WHERE binding_id=? AND generation=? AND pending_generation IS NULL AND status='active'", pending, now(), bindingId, row.generation);
      return true;
    });
    if (!claimed) throw new CredentialBrokerError("refresh_in_progress", "credential refresh generation is no longer current");
    return { material, secret_ref: row.secret_ref, purpose: row.purpose, execution_realm_id: row.execution_realm_id, provider_id: row.provider_id, provider_host: row.provider_host, generation: row.generation };
  }

  /** Provider failure is terminal for this generation; require explicit OAuth again. */
  async markRefreshReauthRequired(bindingIdInput: string, expectedGeneration: number, reason: string): Promise<void> {
    this.schema();
    const bindingId = required(bindingIdInput, "binding_id");
    if (!Number.isSafeInteger(expectedGeneration) || expectedGeneration < 0) throw new CredentialBrokerError("invalid_request", "expected refresh generation is invalid");
    this.ctx.storage.transactionSync(() => {
      const current = this.row(bindingId);
      if (!current || !isCurrentRefreshGeneration(current, expectedGeneration)) return;
      this.ctx.storage.sql.exec("UPDATE credential SET status='reauth_required', updated_at=? WHERE binding_id=? AND generation=? AND pending_generation=? AND status='active'", now(), bindingId, expectedGeneration, expectedGeneration + 1);
    });
    console.warn("credential refresh requires re-authentication", bindingId, safeError(reason));
  }

  private row(bindingId: string): StoredRow | undefined { return this.ctx.storage.sql.exec("SELECT binding_id,secret_ref,purpose,execution_realm_id,provider_id,ciphertext,status,generation,pending_generation,provider_host,expires_at FROM credential WHERE binding_id=?", bindingId).toArray()[0] as StoredRow | undefined; }
  private async encrypt(material: OAuthMaterial, bindingId: string): Promise<string> { try { return await encryptOAuthMaterial(this.env.PILLBOX_CREDENTIAL_ENCRYPTION_KEY ?? "", material, `pillbox-credential/v1:${bindingId}`); } catch (cause) { throw new CredentialBrokerError("encryption_unavailable", "credential encryption is not configured", cause); } }
  private async decrypt(ciphertext: string, bindingId: string): Promise<OAuthMaterial> { try { return await decryptOAuthMaterial<OAuthMaterial>(this.env.PILLBOX_CREDENTIAL_ENCRYPTION_KEY ?? "", ciphertext, `pillbox-credential/v1:${bindingId}`); } catch (cause) { throw new CredentialBrokerError("encryption_unavailable", "credential decryption failed", cause); } }
}

function required(value: unknown, field: string): string { if (typeof value !== "string" || value.length === 0) throw new CredentialBrokerError("invalid_request", `${field} is required`); return value; }
function normalizedHost(value: string): string { const host = required(value, "provider_host").toLowerCase().replace(/\.$/, ""); if (host.includes("/") || host.includes("\\") || host.includes("@") || host.includes(":") || host.includes(" ") || host.includes("?" ) || host.includes("#")) throw new CredentialBrokerError("invalid_request", "provider host must be a hostname"); return host; }
function now(): number { return Math.floor(Date.now() / 1000); }
function safeError(cause: unknown): string { return (cause instanceof Error ? cause.message : String(cause)).replace(/bearer\s+\S+/gi, "bearer [credential redacted]").slice(0, 240); }
