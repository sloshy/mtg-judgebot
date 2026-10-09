-- Where each curated list in the database came from, so the scheduled refresh
-- can bring a built-in list up to date without touching an operator's own.
--
-- One row per list: 'aliases' (card_aliases) and 'notes' (card_notes). source
-- is 'builtin' when the copy of data/*.yaml compiled into the binary was
-- loaded (`judgebot ingest init`, or `aliases` / `notes` with no file) and
-- 'file' when the operator named a file. digest is the SHA-256 of the YAML
-- text loaded, in lowercase hex. A loader writes its row in the transaction
-- that replaces the list's table, so the two never disagree.
--
-- The refresh reloads a 'builtin' list whose digest is not the running
-- binary's copy's and leaves a 'file' list alone. A list with no row (loaded
-- before this table existed) is taken as built-in only when its table holds
-- exactly what the built-in copy loads (crates/bot/src/ingest/lists.rs).
CREATE TABLE curated_lists (
    list      text        PRIMARY KEY CHECK (list IN ('aliases', 'notes')),
    source    text        NOT NULL CHECK (source IN ('builtin', 'file')),
    digest    text        NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    loaded_at timestamptz NOT NULL DEFAULT now()
);
