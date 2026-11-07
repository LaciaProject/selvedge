-- Gradual deployments: a second version takes a share of a script's
-- requests. current_version_id stays the version with the larger share
-- (service bindings, Cron Triggers and queues use it); `split` lists both
-- versions and their percentages, or is NULL for a single version.
ALTER TABLE scripts ADD COLUMN split jsonb;
