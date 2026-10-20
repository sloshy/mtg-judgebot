-- Ratings with reasons and the feedback queue (#27, D38).
--
-- A rating that finds fault (1 incorrect, 2 partial) carries a reason: a
-- category (judge_core::FeedbackCategory) and, optionally, what is wrong and
-- the right answer. A Correct carries none, and a rating from before this
-- migration has none either. A rater is a Discord user (`user_id` their id)
-- or a web browser session (`user_id` `web:<session uuid>`), so the primary
-- key (call_id, user_id) is unchanged. A web rating also keeps the name its
-- rater gave and, for FEEDBACK_IP_DAYS (default 90), the address and user
-- agent it came from: the sweep then clears both and keeps the feedback.
ALTER TABLE ratings
    ADD COLUMN rater_kind text NOT NULL DEFAULT 'discord'
        CHECK (rater_kind IN ('discord', 'web')),
    ADD COLUMN category text
        CHECK (category IN ('wrong_conclusion', 'missed_part', 'bad_citation', 'wrong_card',
                            'misread_question', 'wrong_refusal', 'outdated', 'other')),
    ADD COLUMN what_wrong text CHECK (char_length(what_wrong) BETWEEN 1 AND 1000),
    ADD COLUMN right_answer text CHECK (char_length(right_answer) BETWEEN 1 AND 1000),
    ADD COLUMN web_name text CHECK (char_length(web_name) BETWEEN 1 AND 100),
    ADD COLUMN ip inet,
    ADD COLUMN user_agent text CHECK (char_length(user_agent) BETWEEN 1 AND 512),
    -- Only a fault has a reason, and a reason's texts need its category.
    ADD CONSTRAINT ratings_reason_needs_a_fault
        CHECK (score < 3 OR (category IS NULL AND what_wrong IS NULL AND right_answer IS NULL)),
    ADD CONSTRAINT ratings_texts_need_a_category
        CHECK (category IS NOT NULL OR (what_wrong IS NULL AND right_answer IS NULL)),
    -- The web's fields on web ratings only, and the id says which kind it is.
    ADD CONSTRAINT ratings_web_fields_on_web_ratings
        CHECK (rater_kind = 'web' OR (web_name IS NULL AND ip IS NULL AND user_agent IS NULL)),
    ADD CONSTRAINT ratings_judges_rate_on_discord CHECK (rater_kind = 'discord' OR NOT is_judge),
    ADD CONSTRAINT ratings_user_id_names_its_kind
        CHECK ((rater_kind = 'web') = (user_id LIKE 'web:%'));

-- The sweep finds the addresses older than its window without a scan of
-- every rating.
CREATE INDEX ratings_kept_address_idx ON ratings (ts) WHERE ip IS NOT NULL OR user_agent IS NOT NULL;
-- `/forget` deletes by user.
CREATE INDEX ratings_user_idx ON ratings (user_id);

-- The operator's work queue: one item per rating that was given a reason.
-- Its state lives here rather than on `ratings`, so re-rating an item's
-- rating as Correct cannot silently delete the operator's note on it; a
-- replaced reason reopens the item. An item goes with its rating (`/forget`,
-- the call retention).
CREATE TABLE feedback_queue (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    call_id     uuid NOT NULL,
    user_id     text NOT NULL,
    status      text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved', 'dismissed')),
    note        text CHECK (char_length(note) BETWEEN 1 AND 2000),
    fixed_by    text CHECK (char_length(fixed_by) BETWEEN 1 AND 200),
    opened_at   timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    UNIQUE (call_id, user_id),
    FOREIGN KEY (call_id, user_id) REFERENCES ratings (call_id, user_id) ON DELETE CASCADE,
    CONSTRAINT feedback_queue_resolved_when_closed CHECK ((status = 'open') = (resolved_at IS NULL))
);
CREATE INDEX feedback_queue_opened_idx ON feedback_queue (opened_at DESC, id DESC);
CREATE INDEX feedback_queue_user_idx ON feedback_queue (user_id);

-- Retrieval reads Discord ratings only (D10): an anonymous web rating never
-- moves a call in or out of the prior-call examples. The same columns as
-- before; every rating stored until now is a Discord one, so no standing
-- changes.
CREATE OR REPLACE VIEW calls_rated AS
SELECT c.id AS call_id,
       ((2.0 * 3 + COALESCE(SUM(r.score), 0)) / (3 + COUNT(r.score)))::real AS smoothed_mean,
       COUNT(r.score)::integer AS n,
       (SELECT j.score FROM ratings j
         WHERE j.call_id = c.id AND j.is_judge AND j.rater_kind = 'discord'
         ORDER BY j.ts DESC LIMIT 1) AS judge_score,
       COALESCE(
           (SELECT j.score::real FROM ratings j
             WHERE j.call_id = c.id AND j.is_judge AND j.rater_kind = 'discord'
             ORDER BY j.ts DESC LIMIT 1),
           ((2.0 * 3 + COALESCE(SUM(r.score), 0)) / (3 + COUNT(r.score)))::real
       ) AS effective_score
FROM calls c
LEFT JOIN ratings r ON r.call_id = c.id AND r.rater_kind = 'discord'
GROUP BY c.id;
