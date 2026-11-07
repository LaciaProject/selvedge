-- HTTP pull consumers: a queue's consumer is a Worker or `http_pull`, whose
-- clients pull leased batches over the API and ack or retry them by lease id.
ALTER TABLE queue_consumers
    ADD COLUMN kind                  text NOT NULL DEFAULT 'worker',
    ADD COLUMN visibility_timeout_ms integer NOT NULL DEFAULT 30000;
-- An HTTP pull consumer has no script: NULL, which the foreign key to
-- scripts lets through.
ALTER TABLE queue_consumers ALTER COLUMN script_name DROP NOT NULL;
