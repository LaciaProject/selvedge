-- R2's S3-compatible API signs requests with an access key id and secret.
-- As on Cloudflare, the access key id is the API token's id and the secret is
-- the SHA-256 of the token's value (the hash stored here already).
ALTER TABLE api_tokens ADD COLUMN id text;
UPDATE api_tokens SET id = md5(token_sha256 || random()::text) WHERE id IS NULL;
ALTER TABLE api_tokens ALTER COLUMN id SET NOT NULL;
CREATE UNIQUE INDEX api_tokens_id ON api_tokens (id);
