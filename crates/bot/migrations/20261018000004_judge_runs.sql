-- One row per run of the built-in pipeline (#23, judge_core::run): /judge on
-- Discord, the web app and POST /api/judge, the MCP `judge` tool and
-- `judge-cli judge`, whatever the outcome or audience. An agent-driven session
-- has no row: its model is the agent.
--
-- outcome: judge_core::Outcome's ids. retried: the first synthesis attempt was
-- rejected, so the one retry was attempted (its request may still have failed).
-- tool_round: a billed request carried the lookup_rules result.
--
-- call_id / failed_call_id: what the run left, at most one of them. Deleting
-- the call (retention) or the failed call (its 30-day / 500-row bound) keeps
-- the run, unlinked, so spend and outcome figures survive.
--
-- started_at is the store's clock less the run's measured duration.
CREATE TABLE judge_runs (
    id             uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    started_at     timestamptz NOT NULL,
    finished_at    timestamptz NOT NULL DEFAULT now(),
    interface      text        NOT NULL
        CHECK (interface IN ('discord', 'web', 'mcp', 'cli', 'agent_session')),
    private        boolean     NOT NULL,
    outcome        text        NOT NULL
        CHECK (outcome IN ('answered', 'ambiguous', 'not_found', 'out_of_scope',
                           'bad_citation', 'malformed_citation', 'empty_verdict',
                           'uncited_rules', 'tool_misuse', 'refused', 'at_cap',
                           'upstream')),
    retried        boolean     NOT NULL,
    tool_round     boolean     NOT NULL,
    call_id        uuid        REFERENCES calls (id) ON DELETE SET NULL,
    failed_call_id bigint      REFERENCES failed_calls (id) ON DELETE SET NULL,
    CHECK (call_id IS NULL OR failed_call_id IS NULL),
    CHECK (started_at <= finished_at),
    -- The web has no private option.
    CHECK (NOT private OR interface <> 'web')
);
CREATE INDEX judge_runs_finished_at ON judge_runs (finished_at DESC);
CREATE INDEX judge_runs_call ON judge_runs (call_id) WHERE call_id IS NOT NULL;
-- Pruning failed_calls sets these NULL; without the index each delete scans.
CREATE INDEX judge_runs_failed_call ON judge_runs (failed_call_id)
    WHERE failed_call_id IS NOT NULL;

-- Each billed request of a run, in billing order (judge_core::Charge).
--
-- stage: extraction | synthesis (an attempt, its retry, a truncation rerun) |
-- tool_round (the request carrying the lookup_rules result) | embedding.
-- model: what the response named, which can differ from the configured one
-- under refusal fallbacks.
-- micro_usd: what the reported usage cost at the configured price (0 for a
-- free model). When the response reported no usage, the tokens are NULL and
-- micro_usd is the worst case the spend cap reserved and kept, flagged by
-- cost_estimated: an upper bound, not a measurement (judge_core::Cost). A
-- run's total is an estimate when any of its charges is. An embedding has
-- input tokens only, the others 0.
CREATE TABLE judge_run_charges (
    run_id             uuid     NOT NULL REFERENCES judge_runs (id) ON DELETE CASCADE,
    seq                smallint NOT NULL CHECK (seq >= 0),
    stage              text     NOT NULL
        CHECK (stage IN ('extraction', 'synthesis', 'tool_round', 'embedding')),
    provider           text     NOT NULL,
    model              text     NOT NULL,
    input_tokens       bigint,
    output_tokens      bigint,
    cache_read_tokens  bigint,
    cache_write_tokens bigint,
    micro_usd          bigint   NOT NULL CHECK (micro_usd >= 0),
    cost_estimated     boolean  NOT NULL,
    PRIMARY KEY (run_id, seq),
    CHECK ((input_tokens IS NULL) = (output_tokens IS NULL)
           AND (input_tokens IS NULL) = (cache_read_tokens IS NULL)
           AND (input_tokens IS NULL) = (cache_write_tokens IS NULL)),
    -- Only a request that reported nothing has its cost estimated (a free
    -- model that reported nothing still cost a known 0).
    CHECK (NOT cost_estimated OR input_tokens IS NULL)
);
