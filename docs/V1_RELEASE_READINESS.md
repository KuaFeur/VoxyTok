# VoxyTok V1 release readiness

This checklist separates automated repository validation from checks that require real TikTok and deployment credentials.

## Automated checks

- [ ] Rust formatting, compilation, tests and strict Clippy pass.
- [ ] PostgreSQL integration tests pass on a clean database.
- [ ] Docker image builds successfully.
- [ ] Container smoke test starts embedded PostgreSQL, applies migrations, reaches `/readyz`, rejects unauthenticated API calls and accepts authenticated API calls.
- [ ] Release workflow publishes a versioned image and updates `latest`.

## TikTok developer application checks

These cannot be completed by CI without the application's real credentials and TikTok-approved products/scopes.

- [ ] Login Kit and Display API are enabled for the TikTok application.
- [ ] The exact HTTPS callback URL is registered in TikTok Developer settings.
- [ ] A creator can authorize successfully and the connection status becomes `connected`.
- [ ] A declined authorization becomes `failed` and the same OAuth state cannot be replayed.
- [ ] The first synchronization does not replay the creator's historical videos.
- [ ] A video published after connection is detected and delivered.
- [ ] Access-token refresh succeeds and the rotated refresh token is persisted.
- [ ] Revoked/expired refresh credentials result in `reauthorization_required`.
- [ ] Temporary provider errors retain the connection and use retry backoff.
- [ ] Deleting a TikTok-linked creator revokes its access token through TikTok's official revoke endpoint before local deletion.
- [ ] A transient revoke/refresh/network failure returns `502` and preserves the local creator so deletion can be retried.
- [ ] Missing TikTok provider configuration returns `503` and preserves the local creator.
- [ ] Deleting a creator whose token was already revoked/expired is idempotent and still removes the local record.
- [ ] After deletion, the account no longer appears as connected in the TikTok app's connected-apps settings.

## Deployment and operations

- [ ] `VOXYTOK_DATA_DIR` is backed by persistent storage and survives a container replacement.
- [ ] A restart preserves creators, OAuth state, encrypted credentials, event history and pending deliveries.
- [ ] A backup and restore procedure for the PostgreSQL data directory has been tested.
- [ ] `TOKEN_ENCRYPTION_KEY`, API token, TikTok client secret and webhook secret are backed up securely.
- [ ] The webhook receiver verifies HMAC over the raw request body and deduplicates `Idempotency-Key`.
- [ ] A failed webhook delivery retries; an interrupted `processing` delivery is recovered after restart.
- [ ] The GHCR image is publicly pullable, or private-registry credentials are configured in Pelican.
- [ ] The Pelican Egg has been imported and a fresh instance has been deployed successfully.

## Release decision

Do not call V1 production-validated until every item above that applies to the target deployment has been checked. CI success alone does not prove TikTok app approval, real OAuth behavior, provider rate limits or production restore readiness.
