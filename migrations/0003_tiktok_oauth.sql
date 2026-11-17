-- OAuth credentials are stored encrypted at rest. TOKEN_ENCRYPTION_KEY must
-- remain stable across restarts and backups; losing it requires reconnecting creators.
ALTER TABLE creators
    ADD COLUMN IF NOT EXISTS tiktok_open_id TEXT UNIQUE,
    ADD COLUMN IF NOT EXISTS access_token_enc TEXT,
    ADD COLUMN IF NOT EXISTS refresh_token_enc TEXT,
    ADD COLUMN IF NOT EXISTS access_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS refresh_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_polled_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_error TEXT;

CREATE TABLE IF NOT EXISTS oauth_states (
    state_hash TEXT PRIMARY KEY,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS oauth_states_expiry_idx ON oauth_states (expires_at);
