-- Bounded by the managed invocation allowance. Keep tombstones for the execution
-- idempotency retention window; prune only alongside the corresponding execution.
CREATE TABLE digitalocean_allocations (
  invocation_id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  config_id TEXT NOT NULL,
  provider_session_id TEXT,
  state TEXT NOT NULL CHECK (state IN ('creating', 'ready', 'stopping', 'deleted'))
);
