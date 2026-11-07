-- R2 object lifecycle rules (`wrangler r2 bucket lifecycle`), as the API
-- stores them; NULL means Cloudflare's default rule (abort incomplete
-- multipart uploads after 7 days).
ALTER TABLE r2_buckets ADD COLUMN lifecycle jsonb;
