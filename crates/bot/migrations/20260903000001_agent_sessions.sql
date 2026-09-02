-- Agent-driven judge sessions (crates/bot/src/session): the pipeline run in
-- pull mode, where an outside agent (an MCP client, or a shell agent through
-- judge-cli) does the model's work step by step. The whole state machine —
-- which step is next, the retrieved Context, the pending rejection — is one
-- jsonb document, so a step is a load/modify/save with no protocol session
-- behind it: any process with the id can take the next step, which is what
-- the stateless MCP transport and a one-command-per-step CLI both need.
--
-- `version` is bumped on every save and checked on update, so two concurrent
-- steps on one session cannot both apply (the loser gets a conflict).

CREATE TABLE agent_sessions (
    id          uuid PRIMARY KEY,
    thread_id   text NOT NULL,
    question    text NOT NULL,
    state       jsonb NOT NULL,
    version     integer NOT NULL DEFAULT 1,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL
);
CREATE INDEX agent_sessions_expires_idx ON agent_sessions (expires_at);
