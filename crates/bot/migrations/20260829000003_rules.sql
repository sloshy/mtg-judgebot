-- ARCHITECTURE.md §4: Comprehensive Rules chunks, glossary, category map.

-- One row per rule chunk at rule granularity: "702" (section), "702.19"
-- (rule, with lettered sub-rules folded into body) and optionally "702.19b".
-- `subsection` is the three-digit section ("702"); `parent_id` the enclosing
-- rule ("702.19" for "702.19b", "702" for "702.19", NULL for "702").
CREATE TABLE rules (
    id          text PRIMARY KEY,
    parent_id   text REFERENCES rules(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
    subsection  text NOT NULL,
    heading     text NOT NULL DEFAULT '',
    body        text NOT NULL,
    examples    text[] NOT NULL DEFAULT '{}',
    cr_version  text NOT NULL,                  -- effective date, YYYYMMDD
    embedding   vector(1024),                   -- NULL until `ingest embed` runs
    tsv         tsvector GENERATED ALWAYS AS (
                    to_tsvector('english'::regconfig,
                        heading || ' ' || body || ' ' || immutable_join(examples, ' '))
                ) STORED
);
CREATE INDEX rules_subsection_idx ON rules (subsection);
CREATE INDEX rules_parent_id_idx ON rules (parent_id);
CREATE INDEX rules_tsv_idx ON rules USING gin (tsv);
CREATE INDEX rules_embedding_idx ON rules USING hnsw (embedding vector_cosine_ops);

-- CR glossary entries.
CREATE TABLE glossary (
    term        text PRIMARY KEY,
    text        text NOT NULL,
    cr_version  text NOT NULL,
    embedding   vector(1024),
    tsv         tsvector GENERATED ALWAYS AS (
                    to_tsvector('english'::regconfig, term || ' ' || text)
                ) STORED
);
CREATE INDEX glossary_term_lower_idx ON glossary (lower(term));
CREATE INDEX glossary_tsv_idx ON glossary USING gin (tsv);
CREATE INDEX glossary_embedding_idx ON glossary USING hnsw (embedding vector_cosine_ops);

-- Question categories -> curated CR subsection ids (mirror of data/categories.yaml).
CREATE TABLE categories (
    id          text PRIMARY KEY,               -- snake_case, matches judge_core::Category
    label       text NOT NULL,
    subsections text[] NOT NULL DEFAULT '{}'    -- e.g. {'613','614'}
);
