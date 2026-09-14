CREATE TABLE managed_session_owner (
  session_id TEXT PRIMARY KEY NOT NULL,
  owner_domain TEXT NOT NULL CHECK (
    owner_domain IN ('huddles_workspace', 'public_controller')
  ),
  owner_digest TEXT NOT NULL CHECK (
    owner_digest GLOB 'sha256:*' AND length(owner_digest) = 71
      AND substr(owner_digest, 8) NOT GLOB '*[^0-9a-f]*'
  ),
  created_at_ms INTEGER NOT NULL
);

CREATE TRIGGER immutable_managed_session_owner
BEFORE UPDATE OF session_id, owner_domain, owner_digest ON managed_session_owner
BEGIN
  SELECT RAISE(ABORT, 'managed_session_owner_immutable');
END;

CREATE TABLE managed_execution_reservation (
  invocation_id TEXT PRIMARY KEY NOT NULL,
  session_id TEXT NOT NULL,
  owner_domain TEXT NOT NULL CHECK (
    owner_domain IN ('huddles_workspace', 'public_controller')
  ),
  owner_digest TEXT NOT NULL CHECK (
    owner_digest GLOB 'sha256:*' AND length(owner_digest) = 71
      AND substr(owner_digest, 8) NOT GLOB '*[^0-9a-f]*'
  ),
  execution_request_hash TEXT NOT NULL,
  source TEXT NOT NULL CHECK (
    source IN ('workspace_provision', 'direct_execution')
  ),
  provision_request_digest TEXT,
  status TEXT NOT NULL CHECK (status IN ('provisioning', 'ready', 'failed')),
  error_code TEXT,
  allowance_epoch TEXT NOT NULL,
  allowance_limit INTEGER NOT NULL CHECK (allowance_limit BETWEEN 1 AND 1000),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  CHECK (
    (source = 'workspace_provision' AND provision_request_digest IS NOT NULL)
    OR
    (source = 'direct_execution' AND provision_request_digest IS NULL AND status = 'ready')
  ),
  CHECK (
    (status = 'failed' AND error_code IS NOT NULL)
    OR
    (status != 'failed' AND error_code IS NULL)
  )
);

CREATE INDEX managed_execution_reservation_session_owner
ON managed_execution_reservation (session_id, owner_domain, owner_digest);

-- Both trigger side effects are part of the reservation insert statement. An
-- allowance failure rolls back the new session owner as well as the reservation.
CREATE TRIGGER bind_managed_session_owner
AFTER INSERT ON managed_execution_reservation
BEGIN
  INSERT OR IGNORE INTO managed_session_owner (
    session_id, owner_domain, owner_digest, created_at_ms
  ) VALUES (
    NEW.session_id, NEW.owner_domain, NEW.owner_digest, NEW.created_at_ms
  );
  SELECT CASE WHEN EXISTS (
    SELECT 1 FROM managed_session_owner
    WHERE session_id = NEW.session_id
      AND owner_domain = NEW.owner_domain
      AND owner_digest = NEW.owner_digest
    LIMIT 1
  ) THEN NULL ELSE RAISE(ABORT, 'managed_session_owner_mismatch') END;
END;

CREATE TRIGGER reserve_managed_execution_reservation
AFTER INSERT ON managed_execution_reservation
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

CREATE TRIGGER immutable_managed_reservation_owner
BEFORE UPDATE OF invocation_id, session_id, owner_domain, owner_digest,
  execution_request_hash, source, provision_request_digest,
  allowance_epoch, allowance_limit, created_at_ms
ON managed_execution_reservation
BEGIN
  SELECT RAISE(ABORT, 'managed_execution_reservation_identity_immutable');
END;

DROP TRIGGER reserve_managed_execution_allowance;

ALTER TABLE execution
ADD COLUMN owner_domain TEXT NOT NULL DEFAULT 'legacy_unowned';
ALTER TABLE execution
ADD COLUMN owner_digest TEXT NOT NULL DEFAULT 'legacy_unowned';

CREATE TRIGGER require_managed_execution_reservation
BEFORE INSERT ON execution
BEGIN
  SELECT CASE WHEN EXISTS (
    SELECT 1 FROM managed_execution_reservation
    WHERE invocation_id = NEW.invocation_id
      AND session_id = NEW.session_id
      AND owner_domain = NEW.owner_domain
      AND owner_digest = NEW.owner_digest
      AND execution_request_hash = NEW.request_hash
      AND allowance_epoch = NEW.allowance_epoch
      AND allowance_limit = NEW.allowance_limit
      AND status = 'ready'
    LIMIT 1
  ) THEN NULL ELSE RAISE(ABORT, 'managed_execution_reservation_unavailable') END;
END;

CREATE TRIGGER immutable_execution_owner
BEFORE UPDATE OF owner_domain, owner_digest ON execution
BEGIN
  SELECT RAISE(ABORT, 'managed_execution_owner_immutable');
END;

ALTER TABLE workspace_finalize
ADD COLUMN target_owner_domain TEXT NOT NULL DEFAULT 'legacy_unowned';
ALTER TABLE workspace_finalize
ADD COLUMN target_owner_digest TEXT NOT NULL DEFAULT 'legacy_unowned';

CREATE TRIGGER reject_unowned_workspace_finalize
BEFORE INSERT ON workspace_finalize
WHEN NEW.target_owner_domain = 'legacy_unowned'
  OR NEW.target_owner_digest = 'legacy_unowned'
BEGIN
  SELECT RAISE(ABORT, 'managed_session_owner_unavailable');
END;

CREATE TRIGGER immutable_workspace_finalize_owner
BEFORE UPDATE OF target_owner_domain, target_owner_digest ON workspace_finalize
BEGIN
  SELECT RAISE(ABORT, 'workspace_finalize_owner_immutable');
END;
