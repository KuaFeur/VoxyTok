# VoxyTok V1 — test plan

This checklist is intended for the first real-world validation of a fresh, non-production VoxyTok instance. It does not require TikTok developer credentials.

## 1. Prepare a disposable Pelican instance

- Import `deploy/pelican/egg-voxytok.json`.
- Verify that `ghcr.io/kuafeur/voxytok:latest` can be pulled from the Wings host. GHCR packages are private by default when first published: if the pull returns `unauthorized`, either make the `voxytok` package public in GitHub → Packages → Package settings → Change visibility, or configure authenticated GHCR pulls on Wings.
- Create a new Pelican server; do not reinstall or modify an existing production instance.
- Allocate one TCP port and set `PORT` to the same port inside the container.
- Set a unique `API_TOKEN` (32+ random characters).
- Set a unique `POSTGRES_PASSWORD` (24–128 characters, letters/digits/underscore/hyphen).
- Set `WEBHOOK_URL` to the receiver's HTTPS endpoint and `WEBHOOK_SECRET` to the same 32+ character secret configured in the receiver.
- Persist `/home/container/data`. If `POSTGRES_DATA_DIR` is set, persist that actual database directory as well.
- Do not allocate or expose PostgreSQL port 5432.

Start the server and wait for readiness:

```bash
curl --fail --show-error https://YOUR-TEST-HOST/readyz
```

Expected: HTTP 200 and `"status":"ready"`.

## 2. Verify API authentication and creator management

Set the test instance URL and token in a protected shell:

```bash
export VOXYTOK_URL='https://YOUR-TEST-HOST'
read -rsp 'VoxyTok API token: ' VOXYTOK_API_TOKEN; echo
```

Unauthenticated access must fail:

```bash
curl -sS -o /dev/null -w '%{http_code}\n' "$VOXYTOK_URL/api/v1/creators"
```

Expected: `401`.

Create a creator (replace the username with a public profile you are allowed to monitor):

```bash
curl --fail --show-error \
  -H "Authorization: Bearer $VOXYTOK_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"username":"@creator"}' \
  "$VOXYTOK_URL/api/v1/creators"
```

Expected: HTTP 201, normalized username without `@`, a UUID and a scheduled `next_poll_at`. Repeating the request for the same username must return the same creator ID rather than creating a duplicate.

List creators and copy the returned `id`:

```bash
curl --fail --show-error \
  -H "Authorization: Bearer $VOXYTOK_API_TOKEN" \
  "$VOXYTOK_URL/api/v1/creators"
```

Inspect per-source monitoring state:

```bash
export CREATOR_ID='UUID-FROM-THE-RESPONSE'
curl --fail --show-error \
  -H "Authorization: Bearer $VOXYTOK_API_TOKEN" \
  "$VOXYTOK_URL/api/v1/creators/$CREATOR_ID/sources"
```

An empty list means no source has attempted this creator yet. After the next polling cycle, expect source entries with attempt/success timestamps or a classified error and retry time. A source error is not by itself a service failure if another adapter succeeds or a retry is scheduled.

## 3. Verify webhook signature and durable delivery without waiting for TikTok

This authenticated test event exercises the outbox, HTTP delivery, HMAC signature and receiver. It does not prove that TikTok retrieval works. Generate the test identifiers once and reuse them unchanged when repeating the request to verify deduplication.

```bash
export TEST_VIDEO_ID="voxytok-v1-test-$(date +%s)"
export TEST_PUBLISHED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

curl --fail --show-error -X POST \
  -H "Authorization: Bearer $VOXYTOK_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{
    \"creator_id\": \"$CREATOR_ID\",
    \"platform_video_id\": \"$TEST_VIDEO_ID\",
    \"published_at\": \"$TEST_PUBLISHED_AT\",
    \"payload\": {
      \"event\": \"tiktok.video.published\",
      \"provider\": \"manual_test\",
      \"creator\": {\"username\": \"test_creator\", \"display_name\": \"VoxyTok test\"},
      \"video\": {
        \"id\": \"manual-test\",
        \"title\": \"VoxyTok V1 webhook test\",
        \"description\": \"Synthetic event to validate delivery and signature\",
        \"url\": \"https://www.tiktok.com/\"
      }
    }
  }" \
  "$VOXYTOK_URL/api/v1/events"
```

Expected: HTTP 202 and `duplicate:false`. The configured receiver should accept the signed request and show the test notification. Repeat the exact same curl command without regenerating `TEST_VIDEO_ID` or `TEST_PUBLISHED_AT`: expect HTTP 200 and `duplicate:true`, with no second delivery.

Inspect delivery state:

```bash
curl --fail --show-error \
  -H "Authorization: Bearer $VOXYTOK_API_TOKEN" \
  "$VOXYTOK_URL/api/v1/events"
```

A delivered event should show `delivery.status = "delivered"`. If the receiver is temporarily unavailable, expect retries and an inspectable error; after the retry limit, the delivery can be manually rescheduled with `POST /api/v1/events/{delivery_id}/retry`.

## 4. Verify real TikTok monitoring

1. Add a real public profile and wait for at least one polling cycle.
2. Confirm its first successful fetch establishes a baseline without sending old videos.
3. Publish a new public video on a test creator account you control, or wait for a new publication from the selected profile.
4. Record publication time, detection time, selected source and webhook receipt time.
5. Check `GET /api/v1/creators/{id}/sources` and `GET /api/v1/events`.
6. Repeat over a sustained window and note source outages, rate limits, missed publications and detection latency.

Do not treat the initial baseline as a new-video notification. TikTok's public embed path exposes only a limited recent-video window; a burst of more than ten videos between successful observations can be missed. RSSHub instances are intermediaries over public TikTok pages, not independent official TikTok APIs. Monitoring is best-effort, not guaranteed real-time or complete archival.

## 5. Verify persistence and recovery

1. Add a creator and deliver at least one synthetic webhook event.
2. Stop and start the Pelican server without reinstalling it.
3. Confirm the creator, source state and delivery history remain present.
4. Take a consistent backup while PostgreSQL is stopped.
5. Restore that backup into a separate disposable test instance with the same database configuration.
6. Confirm `/readyz`, authenticated creator listing and event history work after restore.

Never test restore by overwriting the production data directory.

## 6. Acceptance record

- [ ] Fresh Pelican Egg import succeeds.
- [ ] Container starts and `/readyz` returns HTTP 200.
- [ ] Unauthenticated API returns 401.
- [ ] Creator add is idempotent.
- [ ] Source status is visible and retry state is coherent.
- [ ] Synthetic webhook is received with valid HMAC.
- [ ] Duplicate event does not create a second outbox delivery.
- [ ] Webhook retry succeeds after a temporary receiver failure.
- [ ] Real public TikTok profile baseline succeeds.
- [ ] A later real publication reaches the webhook.
- [ ] Restart preserves creators and delivery history.
- [ ] Backup restore succeeds on a separate disposable instance.

V1 should be called production-validated only after the real-profile, webhook, Pelican persistence and restore checks above have been completed.
