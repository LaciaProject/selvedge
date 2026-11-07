-- Collecting static asset files that no version uses. A file's bytes now
-- live at a path unique to its upload (`path`), so deleting a file can never
-- remove a newer upload of the same hash. Files from before (path NULL, at
-- assets/<account>/<hash>) have no recorded references and are never
-- collected.
ALTER TABLE asset_files ADD COLUMN path text;

-- The files each version manifest references.
CREATE TABLE asset_manifest_entries (
    account_id  text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    manifest    text NOT NULL,
    hash        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, manifest, hash)
);
CREATE INDEX asset_manifest_entries_by_hash ON asset_manifest_entries (account_id, hash);
CREATE INDEX script_versions_asset_manifest ON script_versions (account_id, (assets->>'manifest'))
    WHERE assets IS NOT NULL;
