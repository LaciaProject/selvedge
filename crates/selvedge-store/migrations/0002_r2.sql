-- R2: bucket and object metadata. Object bytes live in the object store under
-- paths that are never reused, so replacing an object only swaps metadata.

CREATE TABLE r2_buckets (
    id          text PRIMARY KEY,
    account_id  text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, name)
);

-- Keys sort byte-wise, as R2 lists them. `parts` is the list of blobs that make
-- up the object, in order: one for a single put, one per part for multipart.
CREATE TABLE r2_objects (
    bucket_id      text NOT NULL REFERENCES r2_buckets(id) ON DELETE CASCADE,
    key            text COLLATE "C" NOT NULL,
    version        text NOT NULL,
    size           bigint NOT NULL,
    etag           text NOT NULL,
    uploaded_ms    bigint NOT NULL,
    http_fields    jsonb NOT NULL,
    custom_fields  jsonb NOT NULL,
    checksums      jsonb NOT NULL,
    storage_class  text NOT NULL,
    parts          jsonb NOT NULL,
    PRIMARY KEY (bucket_id, key)
);

CREATE TABLE r2_uploads (
    id             text PRIMARY KEY,
    bucket_id      text NOT NULL REFERENCES r2_buckets(id) ON DELETE CASCADE,
    key            text NOT NULL,
    http_fields    jsonb NOT NULL,
    custom_fields  jsonb NOT NULL,
    storage_class  text NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE r2_upload_parts (
    upload_id  text NOT NULL REFERENCES r2_uploads(id) ON DELETE CASCADE,
    part       integer NOT NULL,
    etag       text NOT NULL,
    size       bigint NOT NULL,
    path       text NOT NULL,
    PRIMARY KEY (upload_id, part)
);

-- Blobs no longer referenced, deleted from the object store after a grace
-- period so that reads already streaming them can finish.
CREATE TABLE blob_garbage (
    path        text PRIMARY KEY,
    created_at  timestamptz NOT NULL DEFAULT now()
);
