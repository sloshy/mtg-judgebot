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
-- the ones older than 30 days or past the newest 500.
--
-- user_id is the Discord user who asked (NULL for the HTTP API and agents).
-- /forget anonymizes that user's rows: user_id becomes NULL and the question,
-- error, first rejection and attempts are replaced, leaving the time, thread
-- and private flag.
CREATE TABLE failed_calls (
    id              bigserial   PRIMARY KEY,
    created_at      timestamptz NOT NULL DEFAULT now(),
    thread_id       text        NOT NULL,
    private         boolean     NOT NULL,
    user_id         text,
    question        text        NOT NULL,
    error           text        NOT NULL,
    first_rejection text,
    attempts        text[]      NOT NULL DEFAULT '{}'
);
CREATE INDEX failed_calls_created_at ON failed_calls (created_at DESC);
CREATE INDEX failed_calls_user ON failed_calls (user_id) WHERE user_id IS NOT NULL;
