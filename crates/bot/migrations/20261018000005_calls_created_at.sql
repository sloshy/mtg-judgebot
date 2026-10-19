-- The retention (JUDGE_CALL_RETENTION_DAYS, ingest::prune) deletes the oldest
-- calls a batch at a time; without this each batch scans the table.
CREATE INDEX calls_created_at_idx ON calls (created_at);
