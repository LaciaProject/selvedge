-- Queues: messages are rows; consumers claim batches with SKIP LOCKED and a
-- lease, so delivery is at least once across any number of dispatchers.
CREATE TABLE queues (
    id          text PRIMARY KEY,
    account_id  text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, name)
);

CREATE TABLE queue_consumers (
    queue_id           text PRIMARY KEY REFERENCES queues(id) ON DELETE CASCADE,
    account_id         text NOT NULL,
    script_name        text NOT NULL,
    batch_size         integer NOT NULL DEFAULT 10,
    max_wait_ms        integer NOT NULL DEFAULT 5000,
    max_retries        integer NOT NULL DEFAULT 3,
    retry_delay_secs   integer NOT NULL DEFAULT 0,
    dead_letter_queue  text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (account_id, script_name) REFERENCES scripts(account_id, name) ON DELETE CASCADE
);

CREATE TABLE queue_messages (
    id            text PRIMARY KEY,
    queue_id      text NOT NULL REFERENCES queues(id) ON DELETE CASCADE,
    body          bytea NOT NULL,
    content_type  text NOT NULL,
    attempts      integer NOT NULL DEFAULT 0,
    visible_at    timestamptz NOT NULL DEFAULT now(),
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX queue_messages_ready ON queue_messages (queue_id, visible_at);
