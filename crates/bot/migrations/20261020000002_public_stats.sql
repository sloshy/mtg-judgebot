-- What a stranger may read (#26, #28): a row that was not asked private, no
-- older than the public window (JUDGE_FEED_DAYS, NULL for none). The one
-- definition, used by every public read: the feed's page and call reads, the
-- web rating check (judge_bot::feed) and the public dashboard's counts of
-- calls and runs (judge_bot::db::stats). `at` is a call's created_at or a
-- run's finished_at.
--
-- A plain SQL function, STABLE (now()), neither STRICT nor SECURITY DEFINER,
-- so the planner inlines it: a query reads as if the predicate were written
-- out, and the feed's partial indexes (WHERE NOT private) still match.
CREATE FUNCTION publicly_visible(private boolean, at timestamptz, window_days integer)
    RETURNS boolean
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT NOT private AND (window_days IS NULL OR at >= now() - make_interval(days => window_days)) $$;

-- The dashboard counts runs by day over a range, public ones alone for the
-- public page: judge_runs_finished_at serves the range; this serves the
-- public page's part of it without reading private runs.
CREATE INDEX judge_runs_public_finished_at ON judge_runs (finished_at) WHERE NOT private;

-- The spend cap's settings the instance runs with (JUDGE_BUDGET_PERIOD,
-- JUDGE_MAX_USD in micro-dollars), recorded by every process that keeps the
-- spend_days ledger (judge_bot::budget::sync) so the operator's report
-- (judge_bot::db::stats::usage) shows the instance's, whoever reads it: the
-- admin page, judge-cli stats or MCP. One row; the last writer's.
CREATE TABLE budget_setting (
    id         boolean     PRIMARY KEY DEFAULT true CHECK (id),
    period     text        NOT NULL CHECK (period IN ('process', 'day', 'month')),
    cap_micro  bigint      NOT NULL CHECK (cap_micro >= 0),
    written_at timestamptz NOT NULL DEFAULT now()
);
