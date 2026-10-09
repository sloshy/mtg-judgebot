-- One row per "did you mean?" prompt a pick has been taken from, so each
-- prompt answers one click. Every other part of a pick lives in Discord: the
-- question in the bot's own message, the asker, audience, span, card and a
-- digest of the message in the button's custom_id. A click is answered by
-- whichever process holds the gateway, after any restart.
--
-- message_id is the Discord message the buttons were on. shown_ms is when
-- that message last changed (its last edit, else its creation), in Unix
-- milliseconds from Discord's own timestamps. digest is the first 8 bytes of
-- the SHA-256 of the prompt's content, as a signed bigint. One message shows
-- a second prompt when a question has a second ambiguous name, and its
-- content differs, so it is a new key, not a double click, even if Discord
-- left the edit time unset.
--
-- No question, answer, user or card is stored, so a private (/judge
-- private:True) question leaves nothing here but a message id, a time and a
-- digest. A digest cannot be turned back into text, though someone holding
-- both the database and a guess at the exact question and asker could check
-- the guess against it. Taking a claim deletes the claims older than a day,
-- by which time their prompts have long expired.
CREATE TABLE pick_claims (
    message_id bigint      NOT NULL,
    shown_ms   bigint      NOT NULL,
    digest     bigint      NOT NULL,
    claimed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (message_id, shown_ms, digest)
);
