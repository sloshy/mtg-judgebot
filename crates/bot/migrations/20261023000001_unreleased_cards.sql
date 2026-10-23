-- Cards that are not out yet (D42).
--
-- The date of a card's first printing (the earliest `released_at` over its
-- printings in Scryfall's default-cards bulk file), written by the cards step.
-- NULL until that step has run under this release: read as released.
ALTER TABLE cards ADD COLUMN first_printed date;

-- The cards a call's reply noted as unreleased and possibly not covered by the
-- CR it was answered under, kept so a stored call shows the note it was given.
-- Calls stored before this column carry none.
ALTER TABLE calls ADD COLUMN unreleased_cards uuid[] NOT NULL DEFAULT '{}';
