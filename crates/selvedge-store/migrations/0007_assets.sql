-- Workers Static Assets. File bytes live in the blob store at
-- `assets/<account>/<hash>`; a version's manifest is a content-addressed blob.

CREATE TABLE asset_files (
    account_id    text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    hash          text NOT NULL,
    content_type  text,
    size          bigint NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, hash)
);

-- One `wrangler deploy`'s upload: the manifest it declared and the hashes it
-- still has to send. Tokens are stored as SHA-256.
CREATE TABLE asset_upload_sessions (
    id                       text PRIMARY KEY,
    account_id               text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    script_name              text NOT NULL,
    manifest                 jsonb NOT NULL,
    pending                  jsonb NOT NULL,
    upload_token_sha256      text NOT NULL UNIQUE,
    completion_token_sha256  text UNIQUE,
    expires_at               timestamptz NOT NULL
);
CREATE INDEX asset_upload_sessions_expiry ON asset_upload_sessions (expires_at);

ALTER TABLE script_versions ADD COLUMN assets jsonb;
