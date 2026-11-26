-- The taxonomy names CR sections by title (D46).
--
-- The sections of the loaded CR: `713. Meld Cards` under the part
-- `7. Additional Rules`. `rules` keeps a section's title only as a heading
-- fallback, so the titles `data/categories.yaml` names are looked up here.
-- Written by the CR load, replaced whole with each release. Empty until a load
-- has run under this release: the first refresh loads the stored release again
-- to fill it, and `categories` keeps the numbers it had until then.
CREATE TABLE rule_sections (
    id          text PRIMARY KEY,               -- three digits, "713"
    title       text NOT NULL,
    part        text NOT NULL DEFAULT '',       -- the part's title, "Additional Rules"
    cr_version  text NOT NULL
);

-- The entries of a category that name nothing in the loaded CR, as the load
-- reported them (`Zones > General`). Kept so the next load can tell an entry
-- that has just stopped resolving from one already reported.
ALTER TABLE categories ADD COLUMN unresolved text[] NOT NULL DEFAULT '{}';
