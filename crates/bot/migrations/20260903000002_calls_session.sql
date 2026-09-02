-- A call persisted from an agent session records the session, uniquely, so
-- that persisting is idempotent *in the database*: a retried or concurrent
-- persist step of the same session hits the unique index and gets the
-- existing call back (INSERT ... ON CONFLICT ... RETURNING id) instead of
-- filing a second copy into the prior-call pool. Calls from the other front
-- doors have no session.

ALTER TABLE calls ADD COLUMN session_id uuid;
CREATE UNIQUE INDEX calls_session_id_idx ON calls (session_id) WHERE session_id IS NOT NULL;
