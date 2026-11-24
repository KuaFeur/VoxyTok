# Production deployment (Pelican)

This runbook describes how to deploy VoxyTok on a Pelican/Wings node. It does not assume access to the production panel or host and does not perform a deployment.

## Verified image references

- Container: `ghcr.io/kuafeur/voxytok:latest`
- Last independently recorded digest: `sha256:ae68d06314752b4a8dfc80dd98cd0a886dfc77963e8c4ecdc076a715cd59f98e`
- Source commit for that digest: `e2ab3b59f378141e35078fa79384ea6123802c09`
- CI run for that digest: [CI #120](https://github.com/KuaFeur/VoxyTok/actions/runs/44390200642)

The digest listed above is the last digest independently recorded in this runbook; it is **not** asserted to be the digest of the latest publication. The CI workflow records the immutable digest, source commit and run URL in each successful publication's GitHub Actions job summary. For a new deployment, open the **Publish latest container** job summary for the exact successful main-branch run and use its `ghcr.io/kuafeur/voxytok@sha256:...` reference. Before deploying, confirm that the summary's source commit matches the intended release commit. Do not deploy using a digest copied from an older run unless you intend to deploy that older image.

The main-branch workflow publishes `:latest` only after Rust checks, the container build and the embedded-PostgreSQL/HTTP smoke test succeed. The release-tag workflow also requires a successful main-branch CI run for the exact commit being tagged. Stable version tags may update `:latest`; prerelease tags such as `v1.0.0-rc.1` publish the prerelease image without replacing `:latest`.

**The package's anonymous-pull visibility has not been verified.** GitHub documents that a newly published container package defaults to private; before using it from Pelican without registry credentials, open the repository's Packages section, open `voxytok`, then Package settings and set visibility to Public. Alternatively, configure authenticated GHCR pulls on the Wings host. See [GitHub's container-registry documentation](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry). The digest is immutable; `:latest` is a moving tag.

For a first non-production deployment, follow the [V1 end-to-end test plan](TESTING_V1.md) before promoting the image to production.

## Pelican configuration

Import [the VoxyTok Egg](../deploy/pelican/egg-voxytok.json) or update the existing server's image and variables without reinstalling or deleting its data.

| Setting | Production value / rule |
| --- | --- |
| Docker image | Prefer a verified immutable digest |
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

The container's persistent application data directory is `/home/container/data` (or the value of `VOXYTOK_DATA_DIR`). By default it contains the embedded PostgreSQL cluster. If `POSTGRES_DATA_DIR` is explicitly set, PostgreSQL stores its cluster at that path instead. Persist and back up the actual PostgreSQL directory in either case; when using the override, that means persisting and backing up both application data and the separate PostgreSQL path if they are different. These paths must survive image changes, restarts and server reinstalls. Never use a reinstall action that clears either path.

The application listens on `PORT` inside the container. Pelican's allocation and any host-side port mapping must lead to that same listener. For the existing reverse-proxy arrangement, keep Nginx pointing at the host's actual Wings-published allocation (currently recorded as `192.168.1.116:8533`); do not expose PostgreSQL port 5432.

## Safe upgrade procedure

1. **Record the current state.** Note the current image reference/digest, Pelican variables (store secrets securely), allocation, volume mounts and Nginx upstream.
2. **Back up before changing the image.** Stop VoxyTok in Pelican, then take a consistent snapshot/backup of the entire persistent application data directory and the actual PostgreSQL data directory (if separate). Keep backups outside those volumes. Do not copy a live PostgreSQL data directory and assume it is consistent.
3. **Update only the container image.** Set the image to a verified digest. Do not reinstall the Egg, recreate the server, clear files or change the persistent mounts.
4. **Keep configuration stable.** Preserve `API_TOKEN`, `POSTGRES_PASSWORD`, webhook settings, database name/user and `PORT`. RC2 removes OAuth credentials and drops the obsolete OAuth schema during migration. Do not generate new secrets as part of a routine image upgrade.
5. **Start the server.** The entrypoint starts the embedded PostgreSQL service, then the Rust process. On startup the app runs migrations, including migration 0008, which removes OAuth-only database objects. Allow startup to finish before sending API traffic.
6. **Check readiness from the host/reverse-proxy network.** Replace `<voxytok-instance-host>` with the hostname of the instance being deployed:

   ```bash
   curl --fail --show-error https://<voxytok-instance-host>/readyz
   ```

   A healthy response is HTTP 200 with `"status":"ready"`. `/healthz` is liveness only; `/readyz` also checks PostgreSQL.

7. **Check authenticated API access without exposing the token in shell history.** Use a protected environment variable or secret manager, and use the same target hostname:

   ```bash
   curl --fail --show-error \
     -H "Authorization: Bearer $VOXYTOK_API_TOKEN" \
     https://<voxytok-instance-host>/api/v1/creators
   ```

   Replace the placeholder with the URL of this same VoxyTok instance. Expect HTTP 200 and a JSON array. Do not paste the token into tickets, chat or command output.
8. **Check application logs and webhook delivery.** Confirm there are no migration, PostgreSQL, permission or webhook errors. If a webhook is configured, verify receipt and HMAC signature at the bot before adding more creators.
9. **Keep the pre-upgrade backup** until the service has remained healthy and the bot has successfully processed a real event.

## Rollback

1. Stop the Pelican server.
2. Preserve a copy of the post-upgrade application and PostgreSQL data directories for diagnosis.
3. **Review the changes made since the pre-upgrade backup before restoring it.** Restoring that backup discards all later database changes, including creators and events added after the backup. Reconcile any state that must be retained from the preserved post-upgrade copy before considering rollback complete. Notifications already sent after the backup may also need reconciliation with the bot/application to avoid missing or duplicate processing.
4. Restore the complete pre-upgrade application and database backups to their original persistent paths, with ownership/permissions appropriate for the Pelican container UID.
5. Set the previous container image digest/reference and start the server.
6. Recheck `/readyz`, authenticated `/api/v1/creators`, logs and webhook delivery.

Restore the matching database backup when rolling back the binary. Do not downgrade the image while leaving a database that may have been changed by a newer migration unless that compatibility has been explicitly verified.

## First deployment checklist

- [ ] Confirm the target server is the intended VoxyTok instance (one instance = one bot/application).
- [ ] Confirm the persistent mount points to `/home/container/data`.
- [ ] If `POSTGRES_DATA_DIR` is set, persist and back up that actual path too.
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
