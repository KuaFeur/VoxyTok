# Public profile monitoring architecture

## Product contract

Normal creator onboarding requires only a public TikTok username. No creator OAuth, account linking, browser cookies, manual video submission or per-creator source configuration is part of the consumer workflow.

The consuming application still authenticates to its own VoxyTok instance with its server-side API token. One VoxyTok instance continues to belong to one bot/application.

## Acquisition paths

Each source returns a normalized profile and newest-first video records. The initial retrieval paths are:

1. **Direct TikTok embed adapter** — requests the public `/embed/@username` page and parses its embedded profile/video state. It does not solve WAF challenges, use cookies, or bypass access controls.
2. **RSSHub-compatible adapters** — query several configured public RSSHub instances using the TikTok user route and parse RSS 2.0 or Atom. Each host has independent health and cooldown state.

Important limitation: RSSHub's TikTok route also obtains its feed from TikTok's public embed page. These are separate retrieval intermediaries and caches, not independent underlying TikTok data providers. They can provide useful fallback when one route or network path is unavailable, but they cannot guarantee bypass of a TikTok-wide block or outage.

The source registry orders adapters and selects the first healthy one. A successful result is normalized before ingestion. PostgreSQL deduplicates by `(creator_id, platform_video_id)`, so the same post found through multiple paths generates one event.

## Rate-limit and resilience policy

There is no honest way to promise that public TikTok endpoints or public RSSHub instances will never rate-limit or block automated access. VoxyTok must not rotate identities, evade CAPTCHAs, spoof user sessions, or bypass access controls.

Instead:

- bounded global concurrency (four creator lookups at a time);
- configurable per-profile polling interval (five minutes by default; operator range 60-3600 seconds);
- per-host source state and cooldowns;
- respect `Retry-After` on HTTP 429;
- exponential cooldown after repeated failures;
- shared database deduplication across all sources;
- skip sources in cooldown and fail over to the next configured retrieval path;
- no aggressive retries after 403, 429, challenge pages or malformed responses;
- preserve error class, latency, success/failure counters and last successful poll.

If every available path is unavailable, the creator remains monitored and retries after backoff. VoxyTok reports degraded monitoring rather than silently claiming real-time coverage.

## Detection and delivery

- First successful lookup establishes a baseline and does not replay the existing catalogue.
- After the first successful baseline, newly observed video IDs are eligible when their publication timestamp is at or after `monitoring_started_at - 2 minutes`. Eligibility is anchored to subscription start, not the last successful retrieval, so delayed or cached source visibility cannot permanently suppress a post.
- Video IDs are deduplicated in PostgreSQL. Undated videos are stored but not emitted because their publication eligibility cannot be established.
- The unique database constraint prevents duplicates.
- New publications enter the existing durable outbox.
- Existing signed webhook delivery, idempotency keys, retries and one-webhook-per-instance behavior remain unchanged.

## Consumer-facing API

The creator workflow is:

`POST /api/v1/creators`

```json
{ "username": "@creator" }
```

The API normalizes the handle, creates or reuses the monitored profile and schedules discovery. OAuth endpoints remain only as temporary legacy compatibility routes; they are not used by this workflow.

## Deployment

The Rust service and PostgreSQL remain in the existing Docker image and Pelican/Pterodactyl Egg. No Redis requirement is introduced. RSSHub fallback instances are configured by the operator default in the Egg; the consuming bot never talks to RSS directly.

## Rollout requirements

Before production deployment:

1. compile, format and run all Rust tests and Clippy;
2. verify the direct TikTok embed parser against fixture snapshots;
3. verify RSSHub route availability and feed parsing against live public instances;
4. test first-sync baseline, delayed posts, source failover, cooldown, concurrency and deduplication against PostgreSQL;
5. record actual detection latency and source failure rates over a sustained test window;
6. update GitBook docs and validate the Docker image on Pelican/Pterodactyl.

No source should be described as unlimited or guaranteed. The goal is minimal user effort and resilient best-effort public monitoring.
