-- One row per data refresh, started by a command (`judge-ingest refresh`, by
-- hand or from cron: trigger 'manual') or by a process's own schedule
-- ('schedule'), so every process can tell when the last one ran and how it
-- went. process names the binary that ran it.
--
-- A run inserts its row when it starts and fills in the rest when it ends, so
-- a row with no finished_at is a run in progress or one whose process died.
-- started_at and finished_at are the database's clock, and ages are computed
-- against now() in SQL, so every process agrees on them. steps is one JSON
-- object per step in the order run, tagged by "step" and "outcome"
-- (crates/bot/src/ingest/runs.rs documents the shape). ok is null until the
-- run finishes, then false if any step failed. About one row a day, so no
-- index: the readers scan it.
CREATE TABLE refresh_runs (
    id          bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    started_at  timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    trigger     text        NOT NULL CHECK (trigger IN ('schedule', 'manual')),
    process     text        NOT NULL,
    cr_before   text,
    cr_after    text,
    steps       jsonb       NOT NULL DEFAULT '[]',
    ok          boolean
);

