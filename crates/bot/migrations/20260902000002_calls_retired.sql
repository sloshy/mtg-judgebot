-- A call is retired when the data it cited no longer supports it.
--
-- Until now a prior call was retrievable only while its cr_version equalled the
-- newest loaded CR: every release retired every call, including the majority
-- whose cited rules had not changed, and nothing at all retired a call whose
-- cited Oracle text or ruling had. The retirement pass (crates/bot/src/db/retire.rs,
-- run by `ingest retire` and nightly by `ingest refresh`) replaces that with the
-- check that admitted the call in the first place: every citation's source must
-- still exist and still contain the quote. It sets these two columns both ways,
-- so a rule whose text is restored brings its calls back.
--
-- cr_version stays as a record of what the call was answered under; it no
-- longer gates retrieval.

ALTER TABLE calls
    ADD COLUMN retired_at     timestamptz,
    ADD COLUMN retired_reason text,
    ADD CONSTRAINT calls_retired_reason_with_at CHECK ((retired_at IS NULL) = (retired_reason IS NULL));

CREATE INDEX calls_live_category_idx ON calls (category) WHERE retired_at IS NULL;
-- cr_version no longer appears in any query predicate.
DROP INDEX calls_cr_version_idx;

-- Until the first pass runs, keep today's rule so that switching the retrieval
-- filter to retired_at cannot surface a stale call in the meantime. The pass
-- re-evaluates these and restores the ones whose citations still hold.
UPDATE calls
   SET retired_at = now(),
       retired_reason = 'answered under CR ' || cr_version || ', not yet re-checked against the current data'
 WHERE cr_version <> (SELECT max(cr_version) FROM rules);
