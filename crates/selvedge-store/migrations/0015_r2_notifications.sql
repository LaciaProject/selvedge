-- R2 event notifications: rules that send a message to a queue when an
-- object in the bucket is created or deleted.
CREATE TABLE r2_notification_rules (
    id          text PRIMARY KEY,
    bucket_id   text NOT NULL REFERENCES r2_buckets(id) ON DELETE CASCADE,
    queue_id    text NOT NULL REFERENCES queues(id) ON DELETE CASCADE,
    prefix      text NOT NULL DEFAULT '',
    suffix      text NOT NULL DEFAULT '',
    actions     text[] NOT NULL,
    description text,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX r2_notification_rules_bucket ON r2_notification_rules (bucket_id);
