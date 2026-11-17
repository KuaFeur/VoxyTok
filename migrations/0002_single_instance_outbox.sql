-- Upgrade databases created by the early multi-consumer prototype.
-- Each deployment now has exactly one configured webhook destination.
-- Preserve creator and video history, but discard legacy per-subscription
-- delivery rows because their target URLs are no longer part of the model.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'deliveries'
          AND column_name = 'subscription_id'
    ) THEN
        DROP TABLE deliveries;
        DROP TABLE subscriptions;

        CREATE TABLE deliveries (
            id UUID PRIMARY KEY,
            video_id UUID NOT NULL UNIQUE REFERENCES videos(id) ON DELETE CASCADE,
            status TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'processing', 'retry', 'delivered', 'failed')),
            attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
            next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_error TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            delivered_at TIMESTAMPTZ
        );

        CREATE INDEX deliveries_pending_idx
            ON deliveries (next_attempt_at, created_at)
            WHERE status IN ('pending', 'retry');
    END IF;
END
$$;
