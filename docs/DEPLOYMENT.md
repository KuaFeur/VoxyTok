# Production deployment (Pelican)

This runbook describes how to deploy VoxyTok on a Pelican/Wings node. It does not assume access to the production panel or host and does not perform a deployment.

## Release currently published

- Container: `ghcr.io/kuafeur/voxytok:latest`
- Published image digest: `sha256:28ab3efb59d851709f7901c43891a66c438854e122b73cf9deecfe0fdb612d5e`
- Source commit: `3b610ff87e0ca63c1b26aa352677346f37e08454`
- GitHub Actions run: [CI #120](https://github.com/KuaFeur/VoxyTok/actions/runs/44289701890)

The image was pushed to GHCR only after Rust checks, the container build and the embedded-PostgreSQL/HTTP smoke test passed. **The package's anonymous-pull visibility has not been verified.** GitHub documents that a newly published container package defaults to private; before using it from Pelican without registry credentials, open the repository's Packages section, open `voxytok`, then Package settings and set visibility to Public. Alternatively, configure authenticated GHCR pulls on the Wings host. See [GitHub's container-registry documentation](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry). The digest is immutable; `:latest` is a moving tag and may point to a newer build after a later push to `main`.

For a reproducible deployment, use the digest reference if the Pelican Docker image field accepts it:

```text
ghcr.io/kuafeur/voxytok@sha256:28ab3efb59d851709f7901c43891a66c438854e122b73cf9deecfe0fdb612d5e
```

Otherwise use `ghcr.io/kuafeur/voxytok:latest` and record the digest shown by the CI publish job before upgrading.

## Pelican configuration

Import [the VoxyTok Egg](../deploy/pelican/egg-voxytok.json) or update the existing server's image and variables without reinstalling or deleting its data.

| Setting | Production value / rule |
| --- | --- |
| Docker image | `ghcr.io/kuafeur/voxytok:latest` or the pinned digest above |
| `PORT` | Must match the port the application is exposed on inside the container. If the Pelican allocation is 8533 inside the container, set `8533`. |
| `API_TOKEN` | Required, at least 32 characters. Generate a new random value and keep it private. |
| `POSTGRES_PASSWORD` | Required, 24–128 characters using only letters, digits, `_` or `-`. Do not rotate it casually; the entrypoint applies it to the embedded database role at startup. |
| `WEBHOOK_URL` | The HTTPS endpoint of the one bot/application attached to this VoxyTok instance. Leave empty only if webhook delivery is intentionally disabled. |
| `WEBHOOK_SECRET` | Recommended; at least 32 characters. Configure the same secret in the receiving bot to verify HMAC-SHA256 signatures. |
| `POSTGRES_DB` | Keep the default `voxytok`. |
| `POSTGRES_USER` | Keep the default `voxytok`. |
| `POSTGRES_PORT` | Keep `5432`; it is internal only and must not be publicly allocated. |
| `PUBLIC_POLL_INTERVAL_SECONDS` | Default `300` (5 minutes); effective range 60–3600 seconds. |
| `RSSHUB_BASE_URLS` | Keep the default HTTPS fallback list unless there is a tested reason to change it. |
| Legacy TikTok OAuth variables | Leave all four at `unset` for username-only public-profile monitoring. If preserving legacy OAuth records, configure all four together and retain the original encryption key. |

The container's persistent data directory is `/home/container/data` (or the value of `VOXYTOK_DATA_DIR`). It contains the embedded PostgreSQL cluster. It must be mounted to persistent storage and must survive image changes, restarts and server reinstalls. Never use a reinstall action that clears this directory.

The application listens on `PORT` inside the container. Pelican's allocation and any host-side port mapping must lead to that same listener. For the existing reverse-proxy arrangement, keep Nginx pointing at the host's actual Wings-published allocation (currently recorded as `192.168.1.116:8533`); do not expose PostgreSQL port 5432.

## Safe upgrade procedure

1. **Record the current state.** Note the current image reference/digest, Pelican variables (store secrets securely), allocation, volume mount and Nginx upstream.
2. **Back up before changing the image.** Stop VoxyTok in Pelican, then take a consistent snapshot/backup of the entire persistent `/home/container/data` directory. Keep the backup outside that volume. Do not copy a live PostgreSQL data directory and assume it is consistent.
3. **Update only the container image.** Set the image to the pinned digest or the published `:latest` tag. Do not reinstall the Egg, recreate the server, clear files or change the persistent mount.
4. **Keep configuration stable.** Preserve `API_TOKEN`, `POSTGRES_PASSWORD`, webhook settings, database name/user, `PORT` and any legacy OAuth values. Do not generate new secrets as part of a routine image upgrade.
5. **Start the server.** The entrypoint starts the embedded PostgreSQL service, then the Rust process. On startup the app runs migrations, including migration 0007. Allow startup to finish before sending API traffic.
6. **Check readiness from the host/reverse-proxy network:**

   ```bash
   curl --fail --show-error https://tiktok.voxy-bot.xyz/readyz
   ```

   A healthy response is HTTP 200 with `"status":"ready"`. `/healthz` is liveness only; `/readyz` also checks PostgreSQL.

7. **Check authenticated API access without exposing the token in shell history.** Use a protected environment variable or secret manager, then call:

   ```bash
   curl --fail --show-error \
     -H "Authorization: Bearer $VOXYTOK_API_TOKEN" \
     https://tiktok.voxy-bot.xyz/api/v1/creators
   ```

   Expect HTTP 200 and a JSON array. Do not paste the token into tickets, chat or command output.
8. **Check application logs and webhook delivery.** Confirm there are no migration, PostgreSQL, permission or webhook errors. If a webhook is configured, verify receipt and HMAC signature at the bot before adding more creators.
9. **Keep the pre-upgrade backup** until the service has remained healthy and the bot has successfully processed a real event.

## Rollback

1. Stop the Pelican server.
2. Preserve a copy of the post-upgrade data directory for diagnosis.
3. Restore the complete pre-upgrade data backup to the same persistent path, with ownership/permissions appropriate for the Pelican container UID.
4. Set the previous container image digest/reference and start the server.
5. Recheck `/readyz`, authenticated `/api/v1/creators`, logs and webhook delivery.

Restore the matching database backup when rolling back the binary. Do not downgrade the image while leaving a database that may have been changed by a newer migration unless that compatibility has been explicitly verified.

## First deployment checklist

- [ ] Confirm the target server is the intended VoxyTok instance (one instance = one bot/application).
- [ ] Confirm the persistent mount points to `/home/container/data`.
- [ ] Confirm the Pelican allocation and `PORT` agree.
- [ ] Set a unique `API_TOKEN` (32+ characters).
- [ ] Set a strong `POSTGRES_PASSWORD` (24–128 URL-safe characters).
- [ ] Configure `WEBHOOK_URL` and a matching `WEBHOOK_SECRET` if notifications are required.
- [ ] Keep PostgreSQL private; do not publish port 5432.
- [ ] Confirm Nginx terminates HTTPS and proxies to the actual Wings allocation.
- [ ] Back up persistent data before the first start/upgrade.
- [ ] Verify `/readyz` and authenticated API access.

## Operational notes

- The first successful profile fetch establishes a baseline; historical videos are not replayed as new notifications.
- Public TikTok retrieval is best-effort. Rate limits, upstream changes and temporary source outages can delay detection.
- The default polling interval is five minutes; it is not a real-time guarantee.
- Keep the persistent PostgreSQL directory and its backups private. It contains application state and may contain legacy OAuth data if that feature was previously used.
