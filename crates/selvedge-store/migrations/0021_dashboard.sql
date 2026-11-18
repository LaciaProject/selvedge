-- The web dashboard (RFC 0003): people from an OpenID Connect provider, the
-- accounts they are members of, and dashboard sessions.

CREATE TABLE users (
    id             text PRIMARY KEY,
    issuer         text NOT NULL,
    subject        text NOT NULL,
    email          text,
    name           text,
    -- From the provider as of the last login: admin by its groups or email,
    -- and its groups (accounts.oidc_group).
    admin          boolean NOT NULL DEFAULT false,
    groups         text[] NOT NULL DEFAULT '{}',
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_login_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (issuer, subject)
);

-- A member is a user, or an invitation by email until someone with that
-- (verified) email logs in for the first time.
CREATE TABLE account_members (
    id          text PRIMARY KEY,
    account_id  text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    user_id     text REFERENCES users(id) ON DELETE CASCADE,
    email       text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (user_id IS NOT NULL OR email IS NOT NULL)
);
CREATE UNIQUE INDEX account_members_user ON account_members (account_id, user_id) WHERE user_id IS NOT NULL;
CREATE UNIQUE INDEX account_members_invitation ON account_members (account_id, lower(email)) WHERE user_id IS NULL;
CREATE INDEX account_members_by_user ON account_members (user_id);
CREATE INDEX account_members_by_email ON account_members (lower(email)) WHERE user_id IS NULL;

-- Everyone in this provider group is a member of the account.
ALTER TABLE accounts ADD COLUMN oidc_group text;

-- The user who created a token, if a user did.
ALTER TABLE api_tokens ADD COLUMN created_by text REFERENCES users(id) ON DELETE SET NULL;

-- A dashboard session: a user's, or an API token's. Only a hash of the
-- cookie's value is stored.
CREATE TABLE dash_sessions (
    id_sha256     text PRIMARY KEY,
    user_id       text REFERENCES users(id) ON DELETE CASCADE,
    token_sha256  text REFERENCES api_tokens(token_sha256) ON DELETE CASCADE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL,
    CHECK ((user_id IS NULL) <> (token_sha256 IS NULL))
);
CREATE INDEX dash_sessions_expiry ON dash_sessions (expires_at);
CREATE INDEX dash_sessions_by_user ON dash_sessions (user_id);
