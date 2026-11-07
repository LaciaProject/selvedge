-- Workflows: account-wide names registered by `wrangler deploy`. Instances
-- live in Durable Objects (Cloudflare's engine, in cluster mode).
CREATE TABLE workflows (
    account_id   text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name         text NOT NULL,
    id           text NOT NULL UNIQUE,
    script_name  text NOT NULL,
    class_name   text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, name)
);
