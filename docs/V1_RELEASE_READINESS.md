# VoxyTok V1 release readiness

V1 means a self-hosted service that can monitor public TikTok profiles by username, detect newly published videos on a best-effort basis, deduplicate them durably, and deliver signed/retryable webhooks to one configured bot/application.

A successful CI run is necessary but does not prove that TikTok's public pages or third-party RSSHub instances are currently reachable, nor does it validate a real Pelican host. Keep those checks separate.

## Automated release gates

- [ ] Rust formatting, compilation, unit tests, PostgreSQL integration tests and strict Clippy pass on the exact release commit.
- [ ] Docker image builds from the committed `Cargo.lock`.
- [ ] Container smoke test starts embedded PostgreSQL, applies migrations, reaches `/readyz`, rejects unauthenticated API calls and accepts authenticated API calls.
- [ ] Release tags use semantic versioning (`v1.0.0`, `v1.0.0-rc.1`).
- [ ] The release workflow refuses to publish unless the exact commit has a successful CI run on `main`.
- [ ] Prerelease tags publish only their version tag and never overwrite stable `:latest`.
- [ ] The published image digest is recorded with the source commit and CI run.

## Public-profile monitoring acceptance

- [ ] Adding a username with or without `@` creates one monitored creator.
- [ ] Invalid handles are rejected; duplicate adds do not create duplicate creator rows.
- [ ] The first successful fetch establishes a baseline and never replays historical videos.
- [ ] A later publication is normalized and enters the durable outbox once.
- [ ] Repeated observations across direct TikTok and RSSHub adapters produce one event per creator/video ID.
- [ ] Undated feed entries are stored only when appropriate and are never emitted as new publications.
- [ ] A delayed source result is evaluated against the subscription start time, not merely the last poll time.
- [ ] A 429 response honors `Retry-After`; host-wide cooldown prevents parallel creators from hammering the same upstream.
- [ ] A failed source falls through to another configured adapter; all-source failure leaves the creator monitored and schedules retry.
- [ ] Private, missing, challenge and malformed profiles do not silently become successful empty baselines.
- [ ] Poll leases recover after process termination and do not permanently strand creators.
- [ ] The configured polling interval and actual detection latency are measured against a real public test profile over a sustained window.

## API and webhook acceptance

- [ ] `POST /api/v1/creators`, `GET /api/v1/creators` and `DELETE /api/v1/creators/{id}` behave as documented.
- [ ] All management routes require the bearer token; comparisons are constant-time after length validation.
- [ ] Duplicate event ingestion is idempotent at the database constraint.
- [ ] Webhooks include a stable `Idempotency-Key` and HMAC-SHA256 signature when a secret is configured.
- [ ] The receiver verifies HMAC against the raw request body and deduplicates the idempotency key.
- [ ] Non-2xx webhook responses retry with bounded backoff and eventually become inspectable terminal failures.
- [ ] A process restart recovers stale `processing` deliveries without losing pending events.
- [ ] A webhook timeout, invalid destination or DNS failure does not strand a delivery.
- [ ] Manual retry resets the delivery attempt budget and schedules immediate redelivery.

## Deployment and operations acceptance

- [ ] GHCR package is confirmed public for anonymous pulls, or authenticated registry pulls are configured on Wings.
- [ ] The Pelican Egg imports successfully and a fresh non-production instance starts with the documented environment variables.
- [ ] `PORT` matches the container listener and the host allocation/Nginx upstream.
- [ ] `VOXYTOK_DATA_DIR` and the actual PostgreSQL data directory (including a separate `POSTGRES_DATA_DIR`, if configured) persist across container replacement.
- [ ] A restart preserves creators, videos, delivery history and pending deliveries.
- [ ] A consistent PostgreSQL backup has been restored to a clean test instance and verified with `/readyz` and authenticated API calls.
- [ ] Logs expose enough context to diagnose polling, source cooldown, migration and webhook failures without leaking bearer tokens or webhook secrets.
- [ ] The Discord.js example can add a creator, receive a webhook and verify its signature.

## TikTok OAuth compatibility

Public username monitoring is the V1 product path and does not require TikTok developer credentials. The legacy OAuth routes remain only for compatibility with old records/integrations; they are not a V1 acceptance dependency. If an operator chooses to retain legacy OAuth records, the encryption key and all required provider credentials must be preserved and separately validated.

## Release decision

Do not describe V1 as production-validated until all automated gates and applicable deployment checks are green, plus a real public-profile monitoring test has completed. Do not claim guaranteed or real-time TikTok coverage: upstream public endpoints and RSSHub instances can rate-limit, change or become unavailable.
