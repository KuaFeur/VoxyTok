-- Public-profile monitoring no longer depends on a TikTok OAuth identity.
-- Keep per-source health/cooldown state separate so failover can be evaluated
-- independently for each acquisition adapter.
ALTER TABLE creators
    ADD COLUMN IF NOT EXISTS monitoring_started_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_successful_poll_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS poll_lease_until TIMESTAMPTZ;

CREATE TABLE IF NOT EXISTS creator_source_states (
    creator_id UUID NOT NULL REFERENCES creators(id) ON DELETE CASCADE,
    source_key TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'healthy'
        CHECK (status IN ('healthy', 'degraded', 'cooldown', 'disabled')),
    last_attempt_at TIMESTAMPTZ,
    last_success_at TIMESTAMPTZ,
    next_attempt_at TIMESTAMPTZ,
    consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    total_failures BIGINT NOT NULL DEFAULT 0 CHECK (total_failures >= 0),
    total_successes BIGINT NOT NULL DEFAULT 0 CHECK (total_successes >= 0),
    last_latency_ms BIGINT,
    last_http_status INTEGER,
    last_error_code TEXT,
    last_error TEXT,
    etag TEXT,
    last_modified TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (creator_id, source_key)
);

CREATE INDEX IF NOT EXISTS creators_public_monitoring_due_idx
    ON creators (next_poll_at, created_at)
    WHERE enabled = TRUE;

CREATE INDEX IF NOT EXISTS creator_source_states_due_idx
    ON creator_source_states (next_attempt_at, status)
    WHERE status <> 'disabled';
