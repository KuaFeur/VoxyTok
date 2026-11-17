ALTER TABLE creators
    ADD COLUMN IF NOT EXISTS next_poll_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS poll_failures INTEGER NOT NULL DEFAULT 0;
ALTER TABLE deliveries
    ADD COLUMN IF NOT EXISTS processing_started_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS creators_due_poll_idx
    ON creators (next_poll_at, last_polled_at)
    WHERE enabled = TRUE AND tiktok_open_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS deliveries_stale_processing_idx
    ON deliveries (processing_started_at)
    WHERE status = 'processing';
