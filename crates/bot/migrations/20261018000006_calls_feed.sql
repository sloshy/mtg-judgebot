-- The public feed (#26, judge_bot::feed) pages through public calls newest
-- first by (created_at, id), all of them or one interface's or one Discord
-- username's. Private calls are never read there, so the indexes leave them
-- out.
CREATE INDEX calls_feed_idx ON calls (created_at, id) WHERE NOT private;
CREATE INDEX calls_feed_interface_idx ON calls (interface, created_at, id) WHERE NOT private;
CREATE INDEX calls_feed_asker_idx ON calls (asker_name, created_at, id)
    WHERE NOT private AND asker_name IS NOT NULL;
