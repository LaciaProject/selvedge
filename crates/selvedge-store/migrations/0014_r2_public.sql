-- Public access to R2 buckets: an r2.dev-style URL (pub-<dev_url_id>.<zone>)
-- and custom domains in the account's zones. Both serve GET and HEAD of
-- objects without credentials, from the data service's S3 listener.
ALTER TABLE r2_buckets
    ADD COLUMN dev_url_id      text UNIQUE,
    ADD COLUMN dev_url_enabled boolean NOT NULL DEFAULT false;

CREATE TABLE r2_custom_domains (
    domain      text PRIMARY KEY,
    bucket_id   text NOT NULL REFERENCES r2_buckets(id) ON DELETE CASCADE,
    zone_id     text NOT NULL REFERENCES zones(id) ON DELETE CASCADE,
    enabled     boolean NOT NULL DEFAULT true,
    min_tls     text NOT NULL DEFAULT '1.0',
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX r2_custom_domains_bucket ON r2_custom_domains (bucket_id);
