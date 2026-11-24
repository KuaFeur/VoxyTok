# Public profile monitoring architecture

## Product contract

Normal creator onboarding requires only a public TikTok username. No creator OAuth, account linking, browser cookies, manual video submission or per-creator source configuration is part of the consumer workflow.

The consuming application still authenticates to its own VoxyTok instance with its server-side API token. One VoxyTok instance continues to belong to one bot/application.

## Acquisition paths

Each source returns a normalized profile and newest-first video records. The initial retrieval paths are:

1. **Direct TikTok embed adapter** — requests the public `/embed/@username` page and parses its embedded profile/video state. It does not solve WAF challenges, use cookies, or bypass access controls.
2. **RSSHub-compatible adapters** — query several configured public RSSHub instances using the TikTok user route and parse RSS 2.0 or Atom. Each host has independent health and cooldown state.

Important limitations:

- TikTok's documented Creator Profile Embed contains a selection of **up to ten of the creator's most recent videos**. This is not a complete video-history API. If more than ten videos are published between successful observations, VoxyTok may miss some of them.
- RSSHub's TikTok route relies on Puppeteer and also obtains its feed from TikTok's public embed page. These are separate retrieval intermediaries and caches, not independent underlying TikTok data providers. They can help with a transient route/network issue, but they cannot guarantee recovery from a TikTok-wide block or an embed-data change.
- Public RSSHub instances are third-party infrastructure. Operators can set `RSSHUB_BASE_URLS` to instances they control; the default public instances are best-effort fallbacks, not a service-level guarantee.

VoxyTok does not represent this acquisition path as an official TikTok API, a complete archive, or guaranteed real-time monitoring. A deployment that requires complete capture should use a supported, authorized data source with an explicit availability contract.

## Rate-limit and resilience policy

There is no honest way to promise that public TikTok endpoints or public RSSHub instances will never rate-limit or block automated access. VoxyTok must not rotate identities, evade CAPTCHAs, spoof user sessions, or bypass access controls.

Instead:

- bounded global concurrency (eight creator lookups at a time);
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

The API normalizes the handle, creates or reuses the monitored profile and schedules discovery. TikTok OAuth is not part of VoxyTok RC2: no OAuth endpoints, developer credentials, account linking or token storage remain.

## Deployment

The Rust service and PostgreSQL remain in the existing Docker image and Pelican/Pterodactyl Egg. No Redis requirement is introduced. TikTok OAuth-only database objects are removed by migration 0008; earlier migration files remain in the repository to preserve upgrade ordering. RSSHub fallback instances are configured by the operator default in the Egg; the consuming bot never talks to RSS directly.

## Rollout requirements

Before production deployment:

1. compile, format and run all Rust tests and Clippy;
2. verify the direct TikTok embed parser against fixture snapshots;
3. verify RSSHub route availability and feed parsing against live public instances;
4. test first-sync baseline, delayed posts, source failover, cooldown, concurrency and deduplication against PostgreSQL;
5. record actual detection latency, missed-publication behavior and source failure rates over a sustained test window;
6. update GitBook docs and validate the Docker image on Pelican/Pterodactyl.

No source should be described as unlimited or guaranteed. The goal is minimal user effort and resilient best-effort public monitoring, with the ten-video embed window and third-party source dependencies treated as explicit limitations.
