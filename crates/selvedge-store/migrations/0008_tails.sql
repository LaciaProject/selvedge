-- `wrangler tail` sessions. Each is one WebSocket; the URL's token is stored
-- as SHA-256. Agents forward trace events only for scripts with a live session.
CREATE TABLE tail_sessions (
    id            text PRIMARY KEY,
    account_id    text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    script_name   text NOT NULL,
    filters       jsonb NOT NULL,
    token_sha256  text NOT NULL UNIQUE,
    creator_ip    text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL
);
CREATE INDEX tail_sessions_by_script ON tail_sessions (account_id, script_name);
CREATE INDEX tail_sessions_expiry ON tail_sessions (expires_at);
