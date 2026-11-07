-- Queue settings, as `wrangler queues update` and `pause-delivery` set them.
ALTER TABLE queues
    ADD COLUMN delivery_paused     boolean NOT NULL DEFAULT false,
    -- Applied to messages sent without their own delay.
    ADD COLUMN delivery_delay_secs integer NOT NULL DEFAULT 0,
    -- Messages older than this are dropped (Cloudflare's default: 4 days).
    ADD COLUMN retention_secs      integer NOT NULL DEFAULT 345600;
