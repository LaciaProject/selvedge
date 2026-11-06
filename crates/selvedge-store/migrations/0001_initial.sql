-- Selvedge schema, version 1. Released migrations are never edited: add a new file.

CREATE TABLE accounts (
    id          text PRIMARY KEY,
    name        text NOT NULL,
    subdomain   text UNIQUE,
    generation  bigint NOT NULL DEFAULT 1,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE api_tokens (
    token_sha256 text PRIMARY KEY,
    account_id   text REFERENCES accounts(id) ON DELETE CASCADE, -- NULL: platform admin
    name         text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE scripts (
    account_id          text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name                text NOT NULL,
    current_version_id  text,
    workers_dev         boolean NOT NULL DEFAULT true,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, name)
);

CREATE TABLE script_versions (
    id                   text PRIMARY KEY,
    account_id           text NOT NULL,
    script_name          text NOT NULL,
    main_module          text NOT NULL,
    compatibility_date   text NOT NULL,
    compatibility_flags  jsonb NOT NULL,
    modules              jsonb NOT NULL,
    bindings             jsonb NOT NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (account_id, script_name) REFERENCES scripts(account_id, name) ON DELETE CASCADE
);
CREATE INDEX script_versions_by_script ON script_versions (account_id, script_name, created_at);

CREATE TABLE script_secrets (
    account_id   text NOT NULL,
    script_name  text NOT NULL,
    name         text NOT NULL,
    ciphertext   bytea NOT NULL,
    PRIMARY KEY (account_id, script_name, name),
    FOREIGN KEY (account_id, script_name) REFERENCES scripts(account_id, name) ON DELETE CASCADE
);

CREATE TABLE kv_namespaces (
    id          text PRIMARY KEY,
    account_id  text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    title       text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, title)
);

-- Keys sort byte-wise, as Cloudflare lists them.
CREATE TABLE kv_entries (
    namespace_id  text NOT NULL REFERENCES kv_namespaces(id) ON DELETE CASCADE,
    key           text COLLATE "C" NOT NULL,
    value         bytea NOT NULL,
    metadata      text,
    expiration    bigint, -- seconds since the epoch
    updated_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (namespace_id, key)
);
CREATE INDEX kv_entries_expiration ON kv_entries (expiration) WHERE expiration IS NOT NULL;

CREATE TABLE worker_domains (
    hostname    text PRIMARY KEY,
    account_id  text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    service     text NOT NULL,
    id          text NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now()
);
