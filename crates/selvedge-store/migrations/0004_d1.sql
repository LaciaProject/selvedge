-- D1 databases. Their data lives in a Durable Object per database.
CREATE TABLE d1_databases (
    id          text PRIMARY KEY,
    account_id  text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, name)
);
