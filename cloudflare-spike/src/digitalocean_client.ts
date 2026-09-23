import { DIGITALOCEAN_MAX_RESULT_BYTES } from "./digitalocean_contract.js";

export interface DigitalOceanSession {
  readonly session_id: string;
  readonly name: string;
  readonly config_id: string;
  readonly status: string;
}
export interface DigitalOceanApi {
  create(name: string, configId: string): Promise<DigitalOceanSession>;
  get(id: string): Promise<DigitalOceanSession>;
  find(name: string): Promise<DigitalOceanSession | null>;
  exec(id: string, argv: readonly string[]): Promise<string>;
  remove(id: string): Promise<void>;
}

/** Official doctl/godo v1.171.2 wire contract. Never retries a side-effecting request. */
export class DigitalOceanClient implements DigitalOceanApi {
  private readonly token: string;
  private readonly fetcher: typeof fetch;
  constructor(token: string, fetcher: typeof fetch = fetch) {
    this.token = token;
    this.fetcher = fetcher;
  }

  private async request(path: string, method: string, body?: unknown): Promise<unknown> {
    const response = await this.fetcher(`https://api.digitalocean.com/v2/agents/sessions${path}`, {
      method,
      headers: { Authorization: `Bearer ${this.token}`, "Content-Type": "application/json" },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      signal: AbortSignal.timeout(30_000),
      redirect: "error",
    });
    // API bodies may echo secrets, manifests or signed URLs. Never surface them as diagnostics.
    if (method === "DELETE" && response.status === 404) {
      await response.body?.cancel();
      return null;
    }
    if (!response.ok) {
      await response.body?.cancel();
      throw new Error(`DigitalOcean ${method} failed (HTTP ${response.status})`);
    }
    if (response.status === 204) return null;
    if (!response.body) throw new Error("DigitalOcean response body is missing");
    const reader = response.body.getReader();
    const chunks: Uint8Array[] = [];
    let length = 0;
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        length += value.byteLength;
        if (length > DIGITALOCEAN_MAX_RESULT_BYTES * 2)
          throw new Error("DigitalOcean response exceeds byte limit");
        chunks.push(value);
      }
    } finally {
      await reader.cancel();
    }
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    try {
      return JSON.parse(new TextDecoder().decode(bytes));
    } catch {
      throw new Error("DigitalOcean returned invalid JSON");
    }
  }

  async create(name: string, configId: string): Promise<DigitalOceanSession> {
    const body = object(
      await this.request("", "POST", { name, config_id: configId, resume_on_topoff: false }),
    );
    const session = decodeSession(body.session);
    if (session.name !== name || session.config_id !== configId)
      throw new Error("DigitalOcean create identity mismatch");
    return session;
  }

  async get(id: string): Promise<DigitalOceanSession> {
    const body = object(await this.request(`/${validId(id)}`, "GET"));
    const session = decodeSession(body.session);
    if (session.session_id !== id) throw new Error("DigitalOcean session identity mismatch");
    return session;
  }

  async find(name: string): Promise<DigitalOceanSession | null> {
    const body = object(await this.request(`?name=${encodeURIComponent(name)}&page_size=2`, "GET"));
    if (!Array.isArray(body.sessions) || body.sessions.length > 1 || body.next_page_token) {
      throw new Error("DigitalOcean allocation lookup is ambiguous");
    }
    if (!body.sessions.length) return null;
    const session = decodeSession(body.sessions[0]);
    if (session.name !== name) throw new Error("DigitalOcean allocation lookup identity mismatch");
    return session;
  }

  async exec(id: string, argv: readonly string[]): Promise<string> {
    const body = object(
      await this.request(`/${validId(id)}/sandbox/exec`, "POST", {
        argv,
        workdir: "/workspace",
        timeout_seconds: 15,
      }),
    );
    if (body.exit_code !== 0 || (body.stdout !== undefined && typeof body.stdout !== "string")) {
      throw new Error("DigitalOcean sandbox command failed");
    }
    const stdout = body.stdout ?? "";
    if (
      typeof stdout !== "string" ||
      new TextEncoder().encode(stdout).byteLength > DIGITALOCEAN_MAX_RESULT_BYTES
    ) {
      throw new Error("DigitalOcean command output exceeds byte limit");
    }
    return stdout;
  }

  async remove(id: string): Promise<void> {
    await this.request(`/${validId(id)}`, "DELETE");
  }
}

export function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    throw new Error("DigitalOcean response is not an object");
  return value as Record<string, unknown>;
}
function validId(value: unknown): string {
  if (typeof value !== "string" || !/^[0-9a-f-]{36}$/.test(value))
    throw new Error("DigitalOcean resource identity is invalid");
  return value;
}
function decodeSession(value: unknown): DigitalOceanSession {
  const s = object(value);
  if (typeof s.name !== "string" || typeof s.status !== "string")
    throw new Error("DigitalOcean session name is missing");
  return {
    session_id: validId(s.session_id),
    name: s.name,
    config_id: validId(s.config_id),
    status: s.status,
  };
}
