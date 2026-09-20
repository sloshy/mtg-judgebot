-- What the long-running processes spent on model calls, per UTC day.
--
-- bot and api each add their own spend here as they go. A budget period
-- (JUDGE_BUDGET_PERIOD=day|month) sums the days of the current period, so the
-- cap is shared between the processes and survives a restart. With no period
-- configured the rows are only a record, which `judge-cli stats` reads.
--
-- The day is the database's clock in UTC, so every process agrees on it.
-- micro_usd is signed: a reservation is written while its call is in flight
-- and the settled cost, usually lower, corrects it on the next write.
CREATE TABLE spend_days (
    day       date   PRIMARY KEY,
    micro_usd bigint NOT NULL DEFAULT 0,
    calls     bigint NOT NULL DEFAULT 0
);
