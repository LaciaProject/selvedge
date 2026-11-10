-- Container applications (RFC 0002): an image and its limits, attached to a
-- Durable Object class. `spec` is the application as the client sent it,
-- which the API answers with; the columns beside it are what Selvedge acts on.
CREATE TABLE container_applications (
    id            text PRIMARY KEY,
    account_id    text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name          text NOT NULL,
    -- The Durable Object namespace: a hash of account, script and class.
    namespace_id  text NOT NULL,
    script_name   text NOT NULL,
    class_name    text NOT NULL,
    image         text NOT NULL,
    max_instances integer NOT NULL,
    vcpu          double precision NOT NULL,
    memory_mib    bigint NOT NULL,
    disk_mb       bigint NOT NULL,
    spec          jsonb NOT NULL,
    version       integer NOT NULL DEFAULT 1,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, name),
    UNIQUE (account_id, namespace_id)
);

-- Rollouts are applied at once; this is their record.
CREATE TABLE container_rollouts (
    id             text PRIMARY KEY,
    application_id text NOT NULL REFERENCES container_applications(id) ON DELETE CASCADE,
    spec           jsonb NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX container_rollouts_application ON container_rollouts (application_id, created_at);
