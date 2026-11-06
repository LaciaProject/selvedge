-- Cron Triggers. `next_at` is the next time each trigger fires; dispatchers
-- claim due rows with SKIP LOCKED, so every firing happens once.
CREATE TABLE script_schedules (
    account_id   text NOT NULL,
    script_name  text NOT NULL,
    cron         text NOT NULL,
    next_at      timestamptz NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, script_name, cron),
    FOREIGN KEY (account_id, script_name) REFERENCES scripts(account_id, name) ON DELETE CASCADE
);
CREATE INDEX script_schedules_due ON script_schedules (next_at);
