-- Only rule-level rows (parent_id IS NULL, e.g. "702.19") are embedded and
-- searched by the vector leg; leaves ("702.19b") are folded into their rule's
-- body. A partial HNSW index over exactly those rows keeps every candidate the
-- index yields usable (no post-filter shrinking the result below LIMIT).
--
-- Correction to the comment in 20260829000003_rules.sql: the parser emits NO
-- three-digit section rows ("702"); `parent_id` is NULL for rule-level rows and
-- the rule id for leaves. A three-digit id given to `lookup_rules` / `by_ids`
-- is expanded to the rule-level rows of that subsection.
DROP INDEX rules_embedding_idx;
CREATE INDEX rules_embedding_idx ON rules USING hnsw (embedding vector_cosine_ops) WHERE parent_id IS NULL;
UPDATE rules SET embedding = NULL WHERE parent_id IS NOT NULL;
