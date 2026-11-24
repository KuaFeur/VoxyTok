-- RC2 removes TikTok OAuth completely. Preserve fields still used by public
-- profile monitoring, then remove OAuth-only tables and credential columns.
ALTER TABLE creators
    ADD COLUMN IF NOT EXISTS last_polled_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_error TEXT;
DROP TABLE IF EXISTS oauth_states;
ALTER TABLE creators
    DROP COLUMN IF EXISTS tiktok_open_id,
    DROP COLUMN IF EXISTS access_token_enc,
    DROP COLUMN IF EXISTS refresh_token_enc,
    DROP COLUMN IF EXISTS access_expires_at,
    DROP COLUMN IF EXISTS refresh_expires_at;
