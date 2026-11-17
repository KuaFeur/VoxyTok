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

Creates a one-time OAuth state and binds it to a caller-generated opaque reference. The reference must be 32-128 URL-safe characters and should be generated with a cryptographically secure random generator. Store its mapping to the consuming application's own user/session before returning the authorization URL to the user.

Example:

```bash
curl -X POST "$VOXYTOK_URL/api/v1/oauth/tiktok/start" \
  -H "Authorization: Bearer $VOXYTOK_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"reference":"a-cryptographically-random-reference-with-at-least-32-chars"}'
```

Response:

```json
{
  "authorization_url": "https://www.tiktok.com/v2/auth/authorize/?...",
  "reference": "a-cryptographically-random-reference-with-at-least-32-chars",
  "expires_in_seconds": 600
}
```

The application should send the URL to the creator. The creator must sign in to TikTok and approve the requested scopes. The URL expires after ten minutes. A reference already used by another OAuth attempt returns HTTP 409; generate a fresh random reference for every attempt.

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

The success response tells the creator that the account is connected and that the page can be closed. The callback does not accept or store Discord-specific identifiers. The consuming application can poll the connection status below and map the returned creator ID to its own local user using the opaque reference.

### `GET /api/v1/oauth/tiktok/connections/{reference}`

Returns the state of an authorization request. This endpoint is protected by the VoxyTok bearer token. The raw reference is hashed before lookup; VoxyTok never stores it in plaintext.

```http
GET /api/v1/oauth/tiktok/connections/your-random-reference
Authorization: Bearer YOUR_API_TOKEN
```

Possible response:

```json
{
  "status": "connected",
  "creator_id": "CREATOR_UUID",
  "display_name": "Creator",
  "expires_at": "2026-11-17T12:00:00Z"
}
```

Statuses: `pending`, `processing`, `connected`, `failed`, `expired` (the authorization link expired without a successful callback), and `reauthorization_required` (the creator's refresh token expired or TikTok permanently rejected it). Temporary network, rate-limit and server errors keep the creator connected and use retry backoff. When reauthorization is required, start a new OAuth attempt with a fresh reference. The reference is one-use and unique per OAuth attempt. Generate a new reference for each attempt. The status record is retained for at least 24 hours after the authorization URL expires.

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

For TikTok-authorized creators, first revokes the access token through TikTok's official OAuth revoke endpoint, then deletes the creator and related videos/deliveries through database foreign-key cascades. If revocation fails transiently, returns HTTP 502 and keeps the creator so the operation can be retried. Use this to stop monitoring and remove stored data.

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


## Initial synchronization

When a creator connects or reconnects, VoxyTok records a publication watermark and starts polling immediately. Existing videos published before that point are not replayed as new events. Subsequent polls use the last successful poll timestamp with a two-second overlap; database deduplication makes the overlap safe. Videos without a valid publication timestamp are ignored rather than being treated as new.
