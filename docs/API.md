# VoxyTok API reference

Base URL example: `https://voxytok.example.com`.

All management routes under `/api/v1` require:

```http
Authorization: Bearer YOUR_API_TOKEN
```

Keep this token on your application server. Do not expose it to users or browser code.

## Health

### `GET /healthz`

Liveness response. Does not guarantee that PostgreSQL is available.

### `GET /readyz`

Checks PostgreSQL and returns HTTP 503 when the database cannot be reached.

## TikTok authorization

### `POST /api/v1/oauth/tiktok/start`

Creates a one-time OAuth state and returns the TikTok authorization URL. No request body is required.

Example:

```bash
curl -X POST "$VOXYTOK_URL/api/v1/oauth/tiktok/start" \
  -H "Authorization: Bearer $VOXYTOK_API_TOKEN"
```

Response:

```json
{
  "authorization_url": "https://www.tiktok.com/v2/auth/authorize/?...",
  "expires_in_seconds": 600
}
```

The application should send the URL to the creator. The creator must sign in to TikTok and approve the requested scopes. The URL expires after ten minutes.

### `GET /oauth/tiktok/callback`

Public callback registered in the TikTok developer console. VoxyTok validates and consumes the one-time state, exchanges the authorization code, checks the `video.list` scope, fetches the authorized profile and stores encrypted tokens.

On success, the browser receives a JSON response similar to:

```json
{
  "connected": true,
  "display_name": "Creator",
  "message": "TikTok account connected. The first synchronization will start shortly."
}
```

The creator is then visible through `GET /api/v1/creators`. The callback does not accept or store Discord-specific identifiers.

## Creators

### `GET /api/v1/creators`

Returns the creators stored by this instance.

### `POST /api/v1/creators`

Creates a local creator record. This does not authorize TikTok and will not be polled by the official provider until the account completes OAuth.

Request:

```json
{ "username": "local_creator" }
```

### `DELETE /api/v1/creators/{id}`

Deletes the creator and related videos/deliveries through database foreign-key cascades. Use this to stop monitoring and remove stored data.

## Events

### `GET /api/v1/events`

Returns up to the latest 100 events, including delivery status, attempts, last error and delivery timestamp.

### `POST /api/v1/events`

Authenticated ingestion endpoint for provider adapters and integration tests.

Request:

```json
{
  "creator_id": "CREATOR_UUID",
  "platform_video_id": "7350000000000000000",
  "published_at": null,
  "payload": {
    "event": "tiktok.video.published",
    "provider": "custom_adapter",
    "creator": { "username": "creator" },
    "video": {
      "id": "7350000000000000000",
      "url": "https://www.tiktok.com/@creator/video/7350000000000000000"
    }
  }
}
```

The `creator_id` must refer to an enabled creator in this instance. A duplicate `(creator_id, platform_video_id)` returns HTTP 200 with `duplicate: true`; a new event returns HTTP 202 and creates one delivery.

## Retry

### `POST /api/v1/events/{delivery_id}/retry`

Schedules a failed or retryable delivery immediately. Returns HTTP 202 when accepted and HTTP 404 if the delivery does not exist or is not in a retryable state.

## Webhook payload

For a TikTok publication, the current provider emits a JSON object in this shape:

```json
{
  "event": "tiktok.video.published",
  "provider": "tiktok_display_api",
  "creator": {
    "id": "CREATOR_UUID",
    "username": "tt_provider_identifier",
    "display_name": "Creator"
  },
  "video": {
    "id": "7350000000000000000",
    "title": "Video title",
    "description": "Video description",
    "duration": 42,
    "cover_image_url": "https://...",
    "url": "https://www.tiktok.com/@creator/video/7350000000000000000",
    "embed_url": "https://..."
  }
}
```

Optional fields may be `null` or absent depending on TikTok's response.

Headers:
- `Idempotency-Key`: stable delivery UUID.
- `X-VoxyTok-Signature`: `sha256=<hex>`, when a webhook secret is configured.

Verify HMAC-SHA256 over the exact raw body before JSON parsing. The delivery guarantee is at-least-once; deduplicate using the idempotency key.
