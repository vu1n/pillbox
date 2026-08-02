import type { Env } from "../worker.js";
import { CredentialBrokerError, type CredentialRefreshResult } from "../credentials/contract.js";

type OAuthState = { state_id: string; installation_id: string; binding_id: string; secret_ref: string; purpose: string; execution_realm_id: string; principal_id: string; provider_id: string; redirect_uri: string; expires_at: number; used: boolean };
export interface OAuthProvider {
  readonly provider_id: string;
  readonly exchange: (input: { readonly state: string; readonly redirect_uri: string; readonly installation_id: string; readonly binding_id: string; readonly principal_id: string }) => Promise<{ readonly provider_host: string; readonly access_token: string; readonly refresh_token?: string; readonly expires_at: number }>;
  readonly refresh?: (material: { readonly access_token: string; readonly refresh_token?: string; readonly expires_at: number; readonly provider_host: string; readonly generation: number }) => Promise<{ readonly access_token: string; readonly refresh_token?: string; readonly expires_at: number }>;
}

/** Provider-neutral callback state. Provider exchange stays inside Pillbox. */
export class OAuthBroker {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;

  private readonly providers: ReadonlyMap<string, OAuthProvider>;

  constructor(ctx: DurableObjectState, env: Env, providers: ReadonlyMap<string, OAuthProvider> = new Map()) { this.ctx = ctx; this.env = env; this.providers = providers; this.ctx.storage.sql.exec(`CREATE TABLE IF NOT EXISTS oauth_state(state_id TEXT PRIMARY KEY, installation_id TEXT NOT NULL, binding_id TEXT NOT NULL, secret_ref TEXT NOT NULL, purpose TEXT NOT NULL, execution_realm_id TEXT NOT NULL, principal_id TEXT NOT NULL, provider_id TEXT NOT NULL, redirect_uri TEXT NOT NULL, expires_at INTEGER NOT NULL, used INTEGER NOT NULL)`); }

  begin(input: { readonly installation_id: string; readonly binding_id: string; readonly secret_ref: string; readonly purpose: string; readonly execution_realm_id: string; readonly principal_id: string; readonly provider_id: string; readonly redirect_uri: string; readonly ttl_seconds?: number }): { state: string; redirect_uri: string } {
    const state = randomId();
    const expires = Math.floor(Date.now() / 1000) + Math.min(input.ttl_seconds ?? 300, 600);
    if (!this.providers.has(required(input.provider_id))) throw new CredentialBrokerError("invalid_request", "OAuth provider is not configured");
    this.ctx.storage.sql.exec("INSERT INTO oauth_state(state_id,installation_id,binding_id,secret_ref,purpose,execution_realm_id,principal_id,provider_id,redirect_uri,expires_at,used) VALUES(?,?,?,?,?,?,?,?,?,?,0)", state, required(input.installation_id), required(input.binding_id), required(input.secret_ref), required(input.purpose), required(input.execution_realm_id), required(input.principal_id), input.provider_id, exactRedirect(input.redirect_uri), expires);
    return { state, redirect_uri: exactRedirect(input.redirect_uri) };
  }

  /** The exchange callback receives no user-provided provider/token material in its result. */
  async complete(input: { readonly state: string; readonly redirect_uri: string }): Promise<{ status: "connected"; binding_id: string; expires_at: number }> {
    const state = this.ctx.storage.sql.exec("SELECT state_id,installation_id,binding_id,secret_ref,purpose,execution_realm_id,principal_id,provider_id,redirect_uri,expires_at,used FROM oauth_state WHERE state_id=?", required(input.state)).toArray()[0] as unknown as OAuthState | undefined;
    if (!state || state.used || state.expires_at <= Math.floor(Date.now() / 1000)) throw new CredentialBrokerError("invalid_request", "OAuth state is invalid or expired");
    if (state.redirect_uri !== exactRedirect(input.redirect_uri)) throw new CredentialBrokerError("invalid_request", "OAuth redirect URI mismatch");
    this.ctx.storage.sql.exec("UPDATE oauth_state SET used=1 WHERE state_id=? AND used=0", state.state_id);
    const provider = this.providers.get(state.provider_id);
    if (!provider) throw new CredentialBrokerError("invalid_request", "OAuth provider is not configured");
    let exchanged;
    try { exchanged = await provider.exchange({ state: state.state_id, redirect_uri: state.redirect_uri, installation_id: state.installation_id, binding_id: state.binding_id, principal_id: state.principal_id }); } catch (cause) { throw new CredentialBrokerError("reauth_required", "provider OAuth exchange failed", cause); }
    const broker = this.env.CredentialBroker;
    if (!broker) throw new CredentialBrokerError("encryption_unavailable", "credential broker is not configured");
    const stub = broker.get(broker.idFromName(state.binding_id));
    const stored = await stub.putOAuthMaterial({ binding_id: state.binding_id, secret_ref: state.secret_ref, purpose: state.purpose, execution_realm_id: state.execution_realm_id, provider_id: state.provider_id, provider_host: exchanged.provider_host, access_token: exchanged.access_token, refresh_token: exchanged.refresh_token, expires_at: exchanged.expires_at });
    return { status: "connected", binding_id: state.binding_id, expires_at: exchanged.expires_at };
  }

  /** Provider refresh stays here; only serializable state crosses the DO RPC. */
  async refresh(input: { readonly binding_id: string }): Promise<CredentialRefreshResult> {
    const broker = this.env.CredentialBroker;
    if (!broker) throw new CredentialBrokerError("encryption_unavailable", "credential broker is not configured");
    const stub = broker.get(broker.idFromName(required(input.binding_id)));
    const current = await stub.beginRefresh(input.binding_id);
    const provider = this.providers.get(current.provider_id);
    if (!provider?.refresh) {
      await stub.markRefreshReauthRequired(input.binding_id, current.generation, "stored OAuth provider refresh is not configured");
      throw new CredentialBrokerError("invalid_request", "stored OAuth provider refresh is not configured");
    }
    try {
      const refreshed = await provider.refresh(current.material);
      const stored = await stub.completeRefresh({ binding_id: input.binding_id, expected_generation: current.generation, provider_id: current.provider_id, ...refreshed });
      return { status: "stored", generation: stored.generation, expires_at: refreshed.expires_at };
    } catch (cause) {
      await stub.markRefreshReauthRequired(input.binding_id, current.generation, safeError(cause));
      return { status: "reauth_required", reason: "ambiguous_refresh" };
    }
  }
}

function required(value: unknown): string { if (typeof value !== "string" || value.length === 0) throw new CredentialBrokerError("invalid_request", "OAuth state field is required"); return value; }
function exactRedirect(value: string): string { const redirect = required(value); const parsed = new URL(redirect); if (parsed.username || parsed.password || parsed.hash) throw new CredentialBrokerError("invalid_request", "OAuth redirect URI is invalid"); return parsed.toString(); }
function randomId(): string { return crypto.randomUUID(); }
function safeError(cause: unknown): string { return cause instanceof Error ? cause.message.replace(/bearer\s+\S+/gi, "bearer [credential redacted]").slice(0, 240) : "provider refresh failed"; }
