export const MAX_MANAGED_REQUEST_BYTES = 1024 * 1024;

export async function readBoundedJson(
  request: Request,
  maximumBytes = MAX_MANAGED_REQUEST_BYTES,
): Promise<unknown> {
  const declared = request.headers.get("content-length");
  if (declared !== null) {
    const bytes = Number(declared);
    if (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > maximumBytes) {
      throw new RequestBodyTooLargeError(maximumBytes);
    }
  }
  if (request.body === null) throw new Error("request body is required");
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    length += value.byteLength;
    if (length > maximumBytes) {
      await reader.cancel();
      throw new RequestBodyTooLargeError(maximumBytes);
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return JSON.parse(
    new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes),
  );
}

export class RequestBodyTooLargeError extends Error {
  readonly code = "request_too_large" as const;

  constructor(maximumBytes: number) {
    super(`request body exceeds ${maximumBytes} bytes`);
    this.name = "RequestBodyTooLargeError";
  }
}
