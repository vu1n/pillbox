import type { AuthorizedCredentialRoute } from "../credentials/contract.js";

export function authorizedProviderHost(host: string, routeHost: string): boolean {
  const normalized = host.toLowerCase().replace(/\.$/, "");
  const allowed = routeHost.toLowerCase().replace(/\.$/, "");
  return normalized === allowed;
}

export function authorizeOutboundRequest(input: { readonly request: Request; readonly route: AuthorizedCredentialRoute }): void {
  const url = new URL(input.request.url);
  if (url.protocol !== "https:" || (url.port !== "" && url.port !== "443") || !authorizedProviderHost(url.hostname, input.route.host) || url.username || url.password) throw new Error("managed outbound request is outside the credential host policy");
}

export function safeProviderRedirect(input: { readonly location: string; readonly baseUrl: string; readonly routeHost: string }): string | undefined {
  try {
    const target = new URL(input.location, input.baseUrl);
    if (target.protocol !== "https:" || (target.port !== "" && target.port !== "443") || !authorizedProviderHost(target.hostname, input.routeHost) || target.username || target.password) return undefined;
    return target.toString();
  } catch {
    return undefined;
  }
}

export function scrubCredentialResponseHeaders(input: { readonly headers: Headers; readonly accessToken: string }): Headers {
  const output = new Headers(input.headers);
  const removableHeaders = new Set<string>();
  for (const [name, value] of output) {
    if (["set-cookie", "authorization", "proxy-authenticate", "proxy-authorization", "www-authenticate", "refresh"].includes(name) || value.includes(input.accessToken)) removableHeaders.add(name);
  }
  for (const name of removableHeaders) output.delete(name);
  return output;
}
