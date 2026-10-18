-- The release of the binary that last migrated this database: `judgebot` at
-- startup, or `judgebot ingest migrate` (judge_bot::db::migrate). Client
-- tools (`judge-cli`, a stdio `judge-mcp`) compare their own release with it
-- (judge_core::release): a newer release of the same major is a notice, a
-- different major, or a different pre-release, stops them.
--
-- One row. A rolled-back image that finds migrations it does not know leaves
-- the row as it is.
CREATE TABLE instance_release (
    id          boolean     PRIMARY KEY DEFAULT true CHECK (id),
    version     text        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT now()
);
