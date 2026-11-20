-- Version 8: a namespace's display name (`archi/06-storage-and-scale.md`, "Namespaces").

-- The customer's label for the end user, shown beside its name. `display_key` is that label
-- lowercase, character by character (`DisplayName::key`), which the prefix of a listing
-- matches: folded by the store's code, the match does not depend on the database's locale.
ALTER TABLE namespaces
    ADD COLUMN display_name text,
    ADD COLUMN display_key text;
