-- Extensions used by every later migration: pgvector (embeddings) and pg_trgm (fuzzy card names).
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- array_to_string() is only STABLE in Postgres, so it cannot feed a generated
-- column. For text[] its output is deterministic; wrap it as IMMUTABLE.
CREATE OR REPLACE FUNCTION immutable_join(parts text[], sep text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE STRICT
AS $$ SELECT array_to_string(parts, sep) $$;
