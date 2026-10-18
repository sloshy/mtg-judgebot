-- The commit the migrating binary was built from (NULL when the build was not
-- stamped), and whether its tree had uncommitted changes, so a client tool's
-- "rebuild from the instance's commit" can name it.
ALTER TABLE instance_release
    ADD COLUMN commit text,
    ADD COLUMN dirty  boolean NOT NULL DEFAULT false;
