-- One row per call that failed for a reason the operator has to diagnose
-- (an exhausted retry, an upstream error, a refusal), with the question and the
-- text the model sent on each synthesis attempt. Without them a log line says
-- "empty verdict" and nothing about what the model wrote.
--
-- A private (/judge private:True) question is stored here too, flagged by
-- `private`: a failure cannot be read without its question. It is the only
-- place a private question is kept, and only when the call failed.
--
-- Bounded on write by PgCallStore::record_failure: the question and each
-- attempt are cut to a fixed number of characters, and inserting a row deletes
-- the ones older than 30 days or past the newest 500. The user who asked is
-- not stored, so /forget has nothing to delete here; rows age out.
CREATE TABLE failed_calls (
    id              bigserial   PRIMARY KEY,
    created_at      timestamptz NOT NULL DEFAULT now(),
    thread_id       text        NOT NULL,
    private         boolean     NOT NULL,
    question        text        NOT NULL,
    error           text        NOT NULL,
    first_rejection text,
    attempts        text[]      NOT NULL DEFAULT '{}'
);
CREATE INDEX failed_calls_created_at ON failed_calls (created_at DESC);
