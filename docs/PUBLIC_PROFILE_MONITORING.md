# Public profile monitoring architecture

## Product contract

Normal creator onboarding must require only a public TikTok username. No creator OAuth, account linking, video submission, browser cookies, or manual source configuration is part of the consumer workflow.

The consuming application still authenticates to its own VoxyTok instance with its server-side API token. One VoxyTok instance continues to belong to one bot/application.

## Acquisition architecture

Acquisition is provider-agnostic. A creator is a public profile identified by a normalized username, not an OAuth identity. The scheduler creates one monitoring job per creator, regardless of how many consumers or webhook destinations exist (one destination per instance).

Each acquisition adapter implements the same contract:
- resolve a username to a canonical public profile;
- fetch a bounded newest-first page of public videos;
- return normalized video metadata and a provider cursor when available;
- classify errors as transient, rate-limited, unavailable, private/not-found, or malformed;
- expose a source identifier and health statistics.

The initial adapters should be independent implementations, not aliases to the same third-party endpoint:
1. **TikTok public embed page adapter** — fetches the public embed representation and parses its embedded profile/video state. This is the primary direct source. It must not attempt to defeat challenges or access controls.
2. **RSS adapter** — consumes RSS 2.0/Atom feeds from a configurable, self-hosted RSSHub-compatible endpoint. This is a fallback/bridge adapter, not the canonical database or a user-facing RSS feature.
3. **Additional adapters** — can be added behind the same trait without changing the public API.

A source registry selects an eligible adapter using health, recent success rate, latency, and cooldown state. A successful result is normalized before ingestion. PostgreSQL deduplicates by (creator_id, platform_video_id), so the same post found by multiple sources generates one event.

## Rate-limit and resilience policy

There is no honest way to promise that public TikTok endpoints will never rate-limit or block automated access. VoxyTok must not rotate identities, evade CAPTCHAs, spoof user sessions, or bypass access controls to claim otherwise.

Instead:
- global and per-host concurrency limits;
- per-profile minimum poll intervals and jitter;
- conditional requests when the source provides validators;
- shared profile cache so the same profile is not fetched twice in one poll window;
- exponential backoff with Retry-After support;
- circuit breakers and temporary source quarantine;
- fallback only to a genuinely independent adapter;
- no aggressive retry loops after 403, 429, challenge pages, or repeated malformed responses;
- metrics for source health, request count, latency, new videos, and failure class.

If every available public source is unavailable, the creator remains monitored and retries after its cooldown. VoxyTok must report degraded monitoring rather than silently claiming real-time coverage.

## Detection and delivery

- First successful lookup establishes a watermark and does not replay the creator's historical catalogue.
- Later lookups inspect newest-first results and stop once they reach the last successful watermark.
- Overlap between polls is intentional; the unique database constraint prevents duplicate events.
- New publications enter the existing durable outbox.
- Existing signed webhook delivery, idempotency keys, retries, and one-webhook-per-instance behavior remain unchanged.

## Consumer-facing API target

The creator workflow should be reduced to POST /api/v1/creators with this body:

    { "username": "@creator" }

The API normalizes the handle, creates or reuses the monitored profile, and schedules discovery. No OAuth start/callback/status endpoints are required for this workflow. Existing webhook payload shape should remain stable so consuming bots do not need platform-specific changes.

## Deployment target

The Rust service and PostgreSQL remain in the existing Docker image and Pelican/Pterodactyl Egg. No Redis requirement is introduced. Any optional RSSHub-compatible endpoint is an internal deployment dependency only; the consumer bot never talks to RSS directly.

## Rollout requirements

Before removing the old OAuth path from a production deployment:
1. implement and fixture-test the public embed parser;
2. implement RSS 2.0 and Atom parsing for the fallback adapter;
3. add migrations for source state, polling leases, watermark, and health counters;
4. add scheduler tests for concurrency, cooldowns, backoff, source failover, and deduplication;
5. validate against public profiles and record actual detection latency and block rates;
6. update API/OpenAPI, Pelican Egg, README, and GitBook docs together.

No source should be described as unlimited or guaranteed. The goal is minimal user effort and resilient best-effort public monitoring.