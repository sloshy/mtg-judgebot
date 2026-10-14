-- The last Discord gateway holder's clean release (discord/gateway.rs, D29).
--
-- A process that takes the gateway lease waits a grace period before it
-- connects, in case the holder before it lost the lease without knowing and
-- is still connected. A holder that closed its gateway at shutdown writes
-- `released_at` on its lease session just before it unlocks, so a standby
-- that takes the lease within a few seconds of that connects at once. Every
-- holder sets it back to NULL as it takes the lease, before it connects, so
-- the row speaks only of the most recent holder. A release older than the
-- window, or a holder that never wrote here (an older release), means the
-- grace as before.
CREATE TABLE gateway_handover (
    one          boolean PRIMARY KEY DEFAULT true CHECK (one),  -- one row, ever
    released_at  timestamptz                                    -- NULL while held, or after an unclean end
);

INSERT INTO gateway_handover DEFAULT VALUES;
