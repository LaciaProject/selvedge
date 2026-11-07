-- Zones and Worker routes (`routes` in wrangler config). A zone is a domain
-- an operator assigned to an account; the account adds route patterns on it.
CREATE TABLE zones (
    id          text PRIMARY KEY,
    account_id  text NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        text NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE worker_routes (
    id          text PRIMARY KEY,
    zone_id     text NOT NULL REFERENCES zones(id) ON DELETE CASCADE,
    pattern     text NOT NULL,
    -- NULL: requests matching the pattern skip Workers.
    script      text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (zone_id, pattern)
);
