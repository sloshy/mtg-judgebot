-- The spend warnings sent this period (JUDGE_SPEND_WARN_AT).
--
-- In a day or month budget every process sees the same total, so each would
-- warn at the same threshold. A process inserts the row first and sends the
-- warning only when its insert landed. The key includes the cap the
-- percentage is of, so a cap raised mid-period, or a process run under
-- another cap (an ingest run with JUDGE_MAX_USD raised), warns at its own.
-- `process` budgets keep their warnings in memory and write nothing here.
-- Rows older than two months are deleted when a new one is claimed.
CREATE TABLE spend_warnings (
    period       text        NOT NULL CHECK (period IN ('day', 'month')),
    period_start date        NOT NULL,
    cap_micro    bigint      NOT NULL,
    percent      smallint    NOT NULL CHECK (percent BETWEEN 1 AND 99),
    sent_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (period, period_start, cap_micro, percent)
);
