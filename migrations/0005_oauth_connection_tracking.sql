-- Generic correlation between an application-side pending action and a TikTok OAuth flow.
-- The raw reference is never stored: only its SHA-256 digest is persisted.
ALTER TABLE oauth_states
    ADD COLUMN IF NOT EXISTS client_reference_hash TEXT,
    ADD COLUMN IF NOT EXISTS creator_id UUID REFERENCES creators(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS consumed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'processing', 'connected', 'failed'));

CREATE UNIQUE INDEX IF NOT EXISTS oauth_states_client_reference_idx
    ON oauth_states (client_reference_hash)
    WHERE client_reference_hash IS NOT NULL;
