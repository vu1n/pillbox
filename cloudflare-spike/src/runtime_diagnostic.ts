/** Operator-safe detail for durable §0 evidence, HTTP errors, and Worker logs. */
export function safeRuntimeDiagnostic(cause: unknown): string {
  const raw =
    cause instanceof Error
      ? `${cause.name}: ${cause.message}`
      : typeof cause === "string"
        ? cause
        : "unknown runtime error";
  return (
    raw
      .replace(/https?:\/\/\S+/gi, "[url redacted]")
      .replace(/\b(?:cfat_|sk-)[A-Za-z0-9_-]+/gi, "[credential redacted]")
      .replace(/[A-Za-z0-9_-]{32,}/g, "[opaque value redacted]")
      .replace(/\s+/g, " ")
      .trim()
      .slice(0, 240) || "unknown runtime error"
  );
}
