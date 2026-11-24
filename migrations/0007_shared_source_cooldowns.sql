-- Shared upstream cooldowns: a rate limit must pause every creator using the same host.
CREATE TABLE IF NOT EXISTS source_host_states (
    host_key TEXT PRIMARY KEY,
    next_attempt_at TIMESTAMPTZ NOT NULL,
    consecutive_failures INTEGER NOT NULL DEFAULT 1 CHECK (consecutive_failures >= 0),
    last_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Older public-profile rows may predate monitoring_started_at. Anchor them
-- to their creation time so post-migration publications remain eligible.
UPDATE creators
SET monitoring_started_at = COALESCE(monitoring_started_at, created_at)
WHERE enabled = TRUE;
