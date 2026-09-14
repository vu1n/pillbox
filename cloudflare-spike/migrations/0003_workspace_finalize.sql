-- Finalize is externally side-effecting (kill, backup, snapshot). The durable
-- claim is written before any of those steps so an uncertain retry can only
-- observe/replay the first attempt, never execute the sequence twice.
CREATE TABLE workspace_finalize (
  finalize_id TEXT PRIMARY KEY NOT NULL,
  session_id TEXT NOT NULL UNIQUE,
  request_digest TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
  result_snapshot TEXT,
  error_code TEXT,
  error_message TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  CHECK (
    (status = 'running' AND result_snapshot IS NULL
      AND error_code IS NULL AND error_message IS NULL)
    OR
    (status = 'completed' AND result_snapshot IS NOT NULL
      AND error_code IS NULL AND error_message IS NULL)
    OR
    (status = 'failed' AND result_snapshot IS NULL
      AND error_code IS NOT NULL AND error_message IS NOT NULL)
  )
);
