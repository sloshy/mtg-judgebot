-- Rulings are identified by content, not by position in Scryfall's list.
--
-- Scryfall gives rulings no id. `idx` was the row's position in the card's
-- ruling list, and that position shifts whenever a ruling is added or removed
-- ahead of it — so a stored citation `(card, idx)` would silently start pointing
-- at a different ruling after a refresh. `key` is judge_core::ruling_key: the
-- first 64 bits of sha256(published_at || '\n' || text) as 16 hex digits. Both
-- fields, because ~95 cards carry the same sentence under two dates. The
-- expression below must equal the Rust function; bot::db::tests pins the two.
-- to_char, not ::text: the cast follows the session's DateStyle.

ALTER TABLE rulings ADD COLUMN key text;
UPDATE rulings
   SET key = left(encode(sha256(convert_to(to_char(published_at, 'YYYY-MM-DD') || E'\n' || text, 'UTF8')), 'hex'), 16);

-- Stored citations move to the new identity while `idx` still exists to map
-- them. A ruling citation whose (card, idx) no longer exists keeps its old
-- shape, which no longer decodes as a Citation; the retirement pass
-- (crates/bot/src/db/retire.rs) retires such a call as unsupported rather than
-- treating it as having cited nothing.
UPDATE calls c
   SET citations = (
       SELECT coalesce(jsonb_agg(
                  CASE WHEN e->>'kind' = 'scryfall_ruling' AND r.key IS NOT NULL
                       THEN (e - 'idx') || jsonb_build_object('ruling', r.key)
                       ELSE e
                  END ORDER BY t.ord), '[]'::jsonb)
         FROM jsonb_array_elements(c.citations) WITH ORDINALITY AS t(e, ord)
         LEFT JOIN rulings r
                ON e->>'kind' = 'scryfall_ruling'
               AND r.oracle_id = (e->>'card')::uuid
               AND r.idx = (e->>'idx')::integer)
 WHERE c.citations @> '[{"kind":"scryfall_ruling"}]'::jsonb;

ALTER TABLE rulings DROP CONSTRAINT rulings_pkey;
ALTER TABLE rulings DROP COLUMN idx;
ALTER TABLE rulings ALTER COLUMN key SET NOT NULL;
ALTER TABLE rulings ADD PRIMARY KEY (oracle_id, key);
