import type { CredentialLease, CredentialLeaseRequest } from "../credentials/contract.js";
import { authorizeOutboundRequest, safeProviderRedirect, scrubCredentialResponseHeaders } from "./policy.js";

/** Trusted Worker-side fetch seam for a Container outbound handler. */
export async function fetchWithCredentialLease(input: { readonly request: Request; readonly leaseRequest: CredentialLeaseRequest; readonly lease: () => Promise<CredentialLease>; readonly fetcher?: typeof fetch }): Promise<Response> {
  authorizeOutboundRequest({ request: input.request, route: input.leaseRequest.route });
  const lease = await input.lease();
  if (lease.provider_host !== input.leaseRequest.route.host.toLowerCase()) throw new Error("credential lease host does not match outbound host");
  const headers = new Headers(input.request.headers);
  headers.delete("authorization");
  headers.delete("proxy-authorization");
  headers.delete("cookie");
  headers.set("authorization", `Bearer ${lease.access_token}`);
  const fetcher = input.fetcher ?? fetch;
  const response = await fetcher(new Request(input.request, { headers, redirect: "manual" }));
  const responseHeaders = scrubCredentialResponseHeaders({ headers: response.headers, accessToken: lease.access_token });
  const location = responseHeaders.get("location");
  if (location !== null) {
    const target = safeProviderRedirect({ location, baseUrl: input.request.url, routeHost: input.leaseRequest.route.host });
    if (target === undefined) {
      return new Response("managed outbound redirect denied\n", { status: 502 });
    }
    responseHeaders.set("location", target);
  }
  // Buffer and scan every response body. A provider may reflect a bearer in a
  // binary or non-text response just as easily as in JSON; returning it to the
  // untrusted container would break the credential boundary.
  const body = new Uint8Array(await response.arrayBuffer());
  const redacted = replaceBytes(body, new TextEncoder().encode(lease.access_token), new TextEncoder().encode("[credential redacted]"));
  responseHeaders.delete("content-length");
  return new Response(redacted, { status: response.status, statusText: "", headers: responseHeaders });
}

function replaceBytes(input: Uint8Array, needle: Uint8Array, replacement: Uint8Array): Uint8Array {
  if (needle.length === 0) return input;
  const output: number[] = [];
  for (let index = 0; index < input.length;) {
    let match = index + needle.length <= input.length;
    for (let offset = 0; match && offset < needle.length; offset += 1) match = input[index + offset] === needle[offset];
    if (match) {
      output.push(...replacement);
      index += needle.length;
    } else {
      output.push(input[index]!);
      index += 1;
    }
  }
  return Uint8Array.from(output);
}
