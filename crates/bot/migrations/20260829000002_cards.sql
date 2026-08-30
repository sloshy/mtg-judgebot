-- ARCHITECTURE.md §4: Scryfall-derived tables plus hand-curated card data.

-- One row per oracle_id (Scryfall bulk oracle-cards). `name` is the full
-- current name ("Fire // Ice" for split cards); faces live in card_faces.
CREATE TABLE cards (
    oracle_id       uuid PRIMARY KEY,
    name            text NOT NULL,
    layout          text NOT NULL,
    type_line       text NOT NULL DEFAULT '',
    cmc             real NOT NULL DEFAULT 0,
    color_identity  text[] NOT NULL DEFAULT '{}',
    keywords        text[] NOT NULL DEFAULT '{}',
    scryfall_id     uuid,                     -- representative printing (for /cards/:id links)
    legalities      jsonb NOT NULL DEFAULT '{}'::jsonb,
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cards_name_trgm_idx ON cards USING gin (name gin_trgm_ops);
CREATE INDEX cards_name_lower_idx ON cards (lower(name));

-- Faces in Scryfall order; single-faced cards have exactly one (face_idx = 0).
CREATE TABLE card_faces (
    oracle_id   uuid NOT NULL REFERENCES cards(oracle_id) ON DELETE CASCADE,
    face_idx    smallint NOT NULL,
    name        text NOT NULL,
    oracle_text text NOT NULL DEFAULT '',
    mana_cost   text NOT NULL DEFAULT '',
    type_line   text NOT NULL DEFAULT '',
    PRIMARY KEY (oracle_id, face_idx)
);
CREATE INDEX card_faces_name_trgm_idx ON card_faces USING gin (name gin_trgm_ops);
CREATE INDEX card_faces_name_lower_idx ON card_faces (lower(name));

-- Every distinct printed name ever seen for an oracle_id (default-cards bulk):
-- old names, errata'd names, flavor names. A printed name may map to several
-- oracle_ids (ambiguity is surfaced to the user, never guessed).
CREATE TABLE printed_names (
    printed_name text NOT NULL,
    oracle_id    uuid NOT NULL REFERENCES cards(oracle_id) ON DELETE CASCADE,
    PRIMARY KEY (printed_name, oracle_id)
);
CREATE INDEX printed_names_trgm_idx ON printed_names USING gin (printed_name gin_trgm_ops);
CREATE INDEX printed_names_lower_idx ON printed_names (lower(printed_name));

-- Scryfall rulings, bulk-loaded from the `rulings` bulk file keyed by oracle_id.
-- `idx` is the position within the card's ruling list (Citation::ScryfallRuling.idx).
CREATE TABLE rulings (
    oracle_id    uuid NOT NULL REFERENCES cards(oracle_id) ON DELETE CASCADE,
    idx          integer NOT NULL,
    published_at date NOT NULL,
    text         text NOT NULL,
    PRIMARY KEY (oracle_id, idx)
);

-- Hand-curated nicknames ("bob" -> Dark Confidant). Aliases are stored lowercased.
CREATE TABLE card_aliases (
    alias     text PRIMARY KEY CHECK (alias = lower(alias) AND alias <> ''),
    oracle_id uuid NOT NULL REFERENCES cards(oracle_id) ON DELETE CASCADE
);
CREATE INDEX card_aliases_oracle_id_idx ON card_aliases (oracle_id);

-- Hand-written notes for "nightmare" cards (Humility, Opalescence, ...).
CREATE TABLE card_notes (
    oracle_id uuid PRIMARY KEY REFERENCES cards(oracle_id) ON DELETE CASCADE,
    note      text NOT NULL
);
