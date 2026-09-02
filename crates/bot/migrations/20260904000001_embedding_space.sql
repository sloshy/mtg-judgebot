-- docs/proposals/providers.md §4.3: which vectors the `embedding` columns hold.
-- Vectors from two models cannot share a column and HNSW needs one fixed width,
-- so the database records the one space its vectors belong to. `ingest embed`
-- writes the row on first use and refuses when its embedder differs; the bot's
-- vector legs go dark (never mix) on a mismatch; `ingest reembed --yes` is the
-- only thing that changes it (and the columns' width) — see db/space.rs.
CREATE TABLE embedding_space (
    one         boolean PRIMARY KEY DEFAULT true CHECK (one),  -- one row, ever
    provider    text NOT NULL,                                 -- judge_embed::Provider: voyage | openai
    model       text NOT NULL,
    dimensions  integer NOT NULL CHECK (dimensions > 0),       -- the columns' vector(N)
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Before this table the only embedder was Voyage at the schema's fixed 1024
-- width, model VOYAGE_MODEL (default voyage-3.5). A database that already holds
-- vectors is labelled so, rather than leaving the first `ingest embed` to label
-- them with whatever it happens to be configured with. An operator who ran with
-- another VOYAGE_MODEL corrects the row by hand:
--   UPDATE embedding_space SET model = 'voyage-3-large';
INSERT INTO embedding_space (provider, model, dimensions)
SELECT 'voyage', 'voyage-3.5', 1024
WHERE EXISTS (SELECT 1 FROM rules WHERE embedding IS NOT NULL)
   OR EXISTS (SELECT 1 FROM glossary WHERE embedding IS NOT NULL)
   OR EXISTS (SELECT 1 FROM calls WHERE embedding IS NOT NULL);
