-- Where a call came from (#23, judge_core::origin): the interface it was asked
-- through, the Discord user who asked, and whether it is private. "Private"
-- means not public-facing: a /judge private:True call (or an agent call made
-- with `private`) is now stored like any other, and kept out of thread
-- history and the prior-call examples. Only the operator's surfaces show it.
--
-- interface: discord | web | mcp | cli | agent_session. Rows written before
-- this migration are labelled from their thread id; an `agent:` thread with no
-- session was the MCP `judge` tool or `judge-cli judge`, which cannot be told
-- apart, and keeps the legacy value `agent`.
--
-- asker_id / asker_name: the Discord user id and unique username, snapshotted
-- when asked (not the display name or a server nickname). NULL for the web,
-- agents and every row written before this. /forget sets both NULL, and on a
-- private call also replaces the question and answer.
--
-- locale: the language the call was asked in, for the translation work (#25).
-- Everything is English today.
--
-- The 2.0.0 upgrade is a stop-then-start deploy, so no 1.2.0 process writes
-- calls while these constraints are added.

ALTER TABLE calls
    ADD COLUMN interface  text,
    ADD COLUMN asker_id   text,
    ADD COLUMN asker_name text,
    ADD COLUMN private    boolean NOT NULL DEFAULT false,
    ADD COLUMN locale     text    NOT NULL DEFAULT 'en';

UPDATE calls SET interface = CASE
    WHEN thread_id LIKE 'web:%' THEN 'web'
    WHEN thread_id LIKE 'agent:%' AND session_id IS NOT NULL THEN 'agent_session'
    WHEN thread_id LIKE 'agent:%' THEN 'agent'
    ELSE 'discord'
END;

ALTER TABLE calls
    ALTER COLUMN interface SET NOT NULL,
    ADD CONSTRAINT calls_interface_check
        CHECK (interface IN ('discord', 'web', 'mcp', 'cli', 'agent_session', 'agent')),
    -- Only Discord knows who asked, and a name is never kept without its id.
    ADD CONSTRAINT calls_asker_check
        CHECK ((asker_id IS NULL OR interface = 'discord')
               AND (asker_name IS NULL OR asker_id IS NOT NULL)),
    -- The web has no private option.
    ADD CONSTRAINT calls_private_check CHECK (NOT private OR interface <> 'web');

-- /forget looks a user's calls up by id.
CREATE INDEX calls_asker_idx ON calls (asker_id) WHERE asker_id IS NOT NULL;

-- Failed calls carry the same origin. `user_id` was already the Discord asker.
ALTER TABLE failed_calls RENAME COLUMN user_id TO asker_id;
ALTER INDEX failed_calls_user RENAME TO failed_calls_asker;
ALTER TABLE failed_calls
    ADD COLUMN interface  text,
    ADD COLUMN asker_name text;
UPDATE failed_calls SET interface = CASE
    WHEN thread_id LIKE 'web:%' THEN 'web'
    WHEN thread_id LIKE 'agent:%' THEN 'agent'
    ELSE 'discord'
END;
ALTER TABLE failed_calls
    ALTER COLUMN interface SET NOT NULL,
    ADD CONSTRAINT failed_calls_interface_check
        CHECK (interface IN ('discord', 'web', 'mcp', 'cli', 'agent_session', 'agent')),
    ADD CONSTRAINT failed_calls_asker_check
        CHECK ((asker_id IS NULL OR interface = 'discord')
               AND (asker_name IS NULL OR asker_id IS NOT NULL));

-- An agent session remembers whether it was begun private, for its persist step.
ALTER TABLE agent_sessions ADD COLUMN private boolean NOT NULL DEFAULT false;
