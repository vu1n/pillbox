-- Constant non-null defaults make these additions safe for populated preview
-- databases. Once the trigger exists, an old Worker using those defaults fails
-- closed because no operator allowance row can match the reserved legacy epoch.
ALTER TABLE execution
ADD COLUMN allowance_epoch TEXT NOT NULL DEFAULT '__pre_allowance__';

ALTER TABLE execution
ADD COLUMN allowance_limit INTEGER NOT NULL DEFAULT 1 CHECK (
  allowance_limit BETWEEN 1 AND 1000
);

-- This is deliberately a singleton, operator-seeded row. Deploying a new epoch
-- and limit is the only reset path; application traffic never creates or resets it.
CREATE TABLE managed_execution_allowance (
  singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
  deployment_epoch TEXT NOT NULL UNIQUE,
  execution_limit INTEGER NOT NULL CHECK (
    execution_limit BETWEEN 1 AND 1000
  ),
  reserved_executions INTEGER NOT NULL DEFAULT 0 CHECK (
    reserved_executions BETWEEN 0 AND execution_limit
  )
);

-- An AFTER INSERT trigger only runs for a genuinely new invocation. SQLite
-- executes the insert and trigger atomically, so a concurrent caller cannot
-- admit work without reserving capacity or reserve capacity for an ignored retry.
CREATE TRIGGER reserve_managed_execution_allowance
AFTER INSERT ON execution
BEGIN
  UPDATE managed_execution_allowance
  SET reserved_executions = reserved_executions + 1
  WHERE singleton = 1
    AND deployment_epoch = NEW.allowance_epoch
    AND execution_limit = NEW.allowance_limit
    AND reserved_executions < execution_limit;
  SELECT CASE changes()
    WHEN 1 THEN NULL
    ELSE RAISE(ABORT, 'managed_execution_allowance_unavailable')
  END;
END;
