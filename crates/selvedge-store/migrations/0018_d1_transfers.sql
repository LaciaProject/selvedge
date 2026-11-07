-- `wrangler d1 export` and `d1 execute --file`: files moving to and from a
-- database through the API, at URLs that carry a random token (stored as its
-- SHA-256) and expire after an hour.
CREATE TABLE d1_transfers (
    token_sha256 text PRIMARY KEY,
    account_id   text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    database_id  text NOT NULL,
    kind         text NOT NULL CHECK (kind IN ('import', 'export')),
    -- Imports: the MD5 the client announced for its file.
    etag         text,
    -- The blob holding the file, once there is one.
    blob_path    text,
    expires_at   timestamptz NOT NULL
);
CREATE INDEX d1_transfers_expiry ON d1_transfers (expires_at);
