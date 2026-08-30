CREATE TABLE execution (
  invocation_id TEXT PRIMARY KEY NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE,
  request_hash TEXT NOT NULL,
  execution_digest TEXT NOT NULL,
  execution_policy_revision TEXT NOT NULL,
  session_id TEXT NOT NULL,
  harness TEXT NOT NULL,
  transport TEXT NOT NULL,
  requested_model TEXT NOT NULL,
  status TEXT NOT NULL CHECK (
    status IN ('running', 'completed', 'failed', 'cancelled', 'interrupted')
  ),
  owner_token TEXT NOT NULL,
  lease_expires_at_ms INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  artifact_key TEXT,
  artifact_media_type TEXT,
  artifact_bytes INTEGER,
  artifact_sha256 TEXT,
  CHECK (
    (status = 'running' AND artifact_key IS NULL AND artifact_media_type IS NULL
      AND artifact_bytes IS NULL AND artifact_sha256 IS NULL)
    OR
    (status != 'running' AND artifact_key IS NOT NULL
      AND artifact_media_type = 'application/json' AND artifact_bytes >= 0
      AND artifact_sha256 IS NOT NULL)
  )
);
