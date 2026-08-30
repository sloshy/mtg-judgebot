-- ARCHITECTURE.md §4: persisted calls and their ratings.

CREATE TABLE calls (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    thread_id   text NOT NULL,
    question    text NOT NULL,
    answer      text NOT NULL,
    category    text NOT NULL,                  -- judge_core::Category snake_case id
    source      text NOT NULL,                  -- cr | commander
    confidence  text NOT NULL DEFAULT 'medium', -- low | medium | high
    citations   jsonb NOT NULL DEFAULT '[]'::jsonb,  -- Vec<Citation> as serialized by judge_core
    context_ids jsonb NOT NULL DEFAULT '{}'::jsonb,  -- {"cards":[uuid],"rules":[id],"rulings":[[uuid,idx]],"prior":[uuid]}
    cr_version  text NOT NULL,
    embedding   vector(1024),
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX calls_thread_id_idx ON calls (thread_id, created_at);
CREATE INDEX calls_category_idx ON calls (category);
CREATE INDEX calls_cr_version_idx ON calls (cr_version);
CREATE INDEX calls_embedding_idx ON calls USING hnsw (embedding vector_cosine_ops);

-- One rating per (call, user); re-rating replaces. 1 incorrect, 2 partial, 3 correct.
CREATE TABLE ratings (
    call_id  uuid NOT NULL REFERENCES calls(id) ON DELETE CASCADE,
    user_id  text NOT NULL,
    score    smallint NOT NULL CHECK (score BETWEEN 1 AND 3),
    is_judge boolean NOT NULL DEFAULT false,
    ts       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (call_id, user_id)
);

-- Bayesian-smoothed mean (prior 2.0, weight 3) over ALL ratings, plus judge
-- override: `judge_score` is the most recent is_judge rating (NULL if none) and
-- `effective_score` = COALESCE(judge_score, smoothed_mean). Every call appears
-- (n = 0, smoothed_mean = 2.0 when unrated).
CREATE VIEW calls_rated AS
SELECT c.id AS call_id,
       ((2.0 * 3 + COALESCE(SUM(r.score), 0)) / (3 + COUNT(r.score)))::real AS smoothed_mean,
       COUNT(r.score)::integer AS n,
       (SELECT j.score FROM ratings j
         WHERE j.call_id = c.id AND j.is_judge
         ORDER BY j.ts DESC LIMIT 1) AS judge_score,
       COALESCE(
           (SELECT j.score::real FROM ratings j
             WHERE j.call_id = c.id AND j.is_judge
             ORDER BY j.ts DESC LIMIT 1),
           ((2.0 * 3 + COALESCE(SUM(r.score), 0)) / (3 + COUNT(r.score)))::real
       ) AS effective_score
FROM calls c
LEFT JOIN ratings r ON r.call_id = c.id
GROUP BY c.id;
