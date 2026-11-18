# VoxyTok API reference

Base URL example: `https://tiktok.voxy-bot.xyz`.

All management routes under `/api/v1` require:

```http
Authorization: Bearer YOUR_API_TOKEN
```

Keep this token on your application server. One VoxyTok instance belongs to one bot or application and has one webhook destination.

## Health

- `GET /healthz`: process liveness.
- `GET /readyz`: PostgreSQL readiness.

Both return `public_monitor_configured` and `public_source_count`, showing whether public acquisition is enabled and how many retrieval paths are configured. The legacy `tiktok_provider_configured` field reports only the old OAuth provider and is not required for username-only monitoring.

## Creators

### `POST /api/v1/creators`

Adds a public TikTok profile for automatic monitoring. **No TikTok OAuth, account linking, cookie, manual video submission or user-side source configuration is required.**

Request:

```json
{ "username": "@creator" }
```

The username may include or omit the leading `@`. VoxyTok normalizes the handle, creates the profile record and schedules its first lookup immediately.

Response (HTTP 201):

```json
{
  "id": "CREATOR_UUID",
  "username": "creator",
  "display_name": null,
  "enabled": true,
  "created_at": "2026-11-18T00:00:00Z",
  "last_successful_poll_at": null,
  "next_poll_at": "2026-11-18T00:00:00Z",
  "poll_failures": 0,
  "last_error": null
}
```

The first successful lookup establishes a baseline and does not replay old videos. New videos discovered by later lookups are inserted into the durable event outbox.

### `GET /api/v1/creators`

Returns creators monitored by this VoxyTok instance.

### `GET /api/v1/creators/{id}/sources`

Returns the per-source monitoring state for a creator. Each entry includes the adapter key, health/cooldown status, last attempt and success timestamps, next retry time, failure/success counters, last latency, HTTP status and most recent error details. An empty array means no source has attempted that creator yet. Returns HTTP 404 if the creator does not exist.

### `DELETE /api/v1/creators/{id}`

Stops monitoring and deletes the creator, its source health state, video history and pending deliveries. Legacy OAuth-connected creators are still revoked through TikTok before deletion when their legacy credentials are configured.

## Events

### `GET /api/v1/events`

Returns up to the latest 100 events, including delivery state and retry information.

### `POST /api/v1/events`

Authenticated ingestion endpoint for internal adapters and integration tests.

Request:

```json
{
  "creator_id": "CREATOR_UUID",
  "platform_video_id": "7350000000000000000",
  "published_at": null,
  "payload": {
    "event": "tiktok.video.published",
    "provider": "tiktok_public_profile",
    "creator": { "username": "creator" },
    "video": {
      "id": "7350000000000000000",
      "url": "https://www.tiktok.com/@creator/video/7350000000000000000"
    }
  }
}
```

A duplicate `(creator_id, platform_video_id)` returns HTTP 200 with `duplicate: true`. A new event returns HTTP 202 and creates one delivery.

### `POST /api/v1/events/{delivery_id}/retry`

Schedules a failed or retryable webhook delivery immediately.

## Webhook contract

For a new publication, VoxyTok sends. The `provider` field is the **source adapter key that produced the event**, not a fixed platform identifier:
- `tiktok_public_profile` for the direct TikTok public embed adapter.
- `rsshub:<hostname>` for an RSSHub fallback adapter (for example, `rsshub:rsshub.app`).

For a new publication, VoxyTok sends:

```json
{
  "event": "tiktok.video.published",
  "provider": "tiktok_public_profile",
  "creator": {
    "id": "CREATOR_UUID",
    "username": "creator",
    "display_name": "Creator"
  },
  "video": {
    "id": "7350000000000000000",
    "title": "Video title",
    "description": "Video description",
    "duration": null,
    "cover_image_url": "https://...",
    "url": "https://www.tiktok.com/@creator/video/7350000000000000000",
    "embed_url": null,
    "published_at": "2026-11-18T00:00:00Z"
  }
}
```

Headers:
- `Idempotency-Key`: stable delivery UUID.
- `X-VoxyTok-Signature`: `sha256=<hex>` when `WEBHOOK_SECRET` is configured.

Verify HMAC-SHA256 over the exact raw body. Delivery is at-least-once; deduplicate using the idempotency key.

## Internal acquisition model

RSS is an internal acquisition format only. VoxyTok does not expose an RSS URL to consuming bots. The service polls independent adapters, normalizes their results and deduplicates by creator/video ID before webhook delivery.

The initial retrieval paths are:
- TikTok public embed hydration data.
- RSSHub-compatible RSS/Atom feeds from multiple configured instances (fallback). RSSHub's route also uses TikTok's embed page, so the instances are separate intermediaries/caches rather than independent TikTok data providers.

**Coverage limitation:** TikTok documents that Creator Profile Embed contains up to ten recent videos. This is not a complete video-history API; a burst of more than ten posts between successful observations can result in missed events. Public RSSHub instances are third-party infrastructure and do not remove this limitation or provide an availability guarantee. Operators may configure instances they control through `RSSHUB_BASE_URLS`.

The scheduler applies source-specific cooldowns, honors `Retry-After`, backs off after failures and skips sources in cooldown. It never claims that any public source is unlimited or guaranteed. A TikTok challenge, profile privacy change or upstream outage can delay detection.

Default polling interval is five minutes (configurable by the operator between 60 and 3600 seconds). This is best-effort polling, not a guaranteed real-time push feed.

## Legacy OAuth routes

The former `POST /api/v1/oauth/tiktok/start`, `GET /api/v1/oauth/tiktok/connections/{reference}` and `GET /oauth/tiktok/callback` routes remain temporarily for existing integrations and migration compatibility. They are not used by the public-profile workflow and will be removed in a later breaking release after migration.
