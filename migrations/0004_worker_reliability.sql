ALTER TABLE creators
    ADD COLUMN IF NOT EXISTS next_poll_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS poll_failures INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS last_polled_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_error TEXT;
ALTER TABLE deliveries
    ADD COLUMN IF NOT EXISTS processing_started_at TIMESTAMPTZ;
DROP INDEX IF EXISTS creators_due_poll_idx;
CREATE INDEX creators_due_poll_idx
    ON creators (next_poll_at, last_polled_at)
    WHERE enabled = TRUE;
CREATE INDEX IF NOT EXISTS deliveries_stale_processing_idx
    ON deliveries (processing_started_at)
    WHERE status = 'processing';
