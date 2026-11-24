use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use rand::{rngs::OsRng, RngCore};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinSet;
use uuid::Uuid;
use crate::sources::{PublicProfile, SourceAdapter, SourceFailure};

#[derive(Clone, FromRow)]
struct DueCreator {
    id: Uuid,
    username: String,
    display_name: Option<String>,
    last_polled_at: Option<DateTime<Utc>>,
    monitoring_started_at: Option<DateTime<Utc>>,
    poll_failures: i32,
}

fn retry_seconds(failures: i32) -> i64 {
    60_i64.saturating_mul(1_i64 << failures.clamp(0, 6)).min(3600)
}

fn failure_details(error: &SourceFailure) -> (String, String, i64) {
    match error {
        SourceFailure::RateLimited { retry_after } => (
            "rate_limited".to_owned(), "source returned HTTP 429".to_owned(),
            retry_after.map(|v| v.as_secs().clamp(60, 2_592_000) as i64).unwrap_or(900),
        ),
        SourceFailure::NotFound => ("not_found".to_owned(), "public profile was not found".to_owned(), 3600),
        SourceFailure::Challenge => ("challenge".to_owned(), "source returned an access challenge".to_owned(), 3600),
        SourceFailure::Malformed(message) => ("malformed".to_owned(), message.chars().take(500).collect(), 900),
        SourceFailure::TemporarilyUnavailable(message) => ("unavailable".to_owned(), message.chars().take(500).collect(), 300),
    }
}

async fn source_is_cooling_down(db: &PgPool, creator_id: Uuid, source_key: &str) -> anyhow::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM creator_source_states WHERE creator_id=$1 AND source_key=$2 AND status='cooldown' AND next_attempt_at>NOW())"
    ).bind(creator_id).bind(source_key).fetch_one(db).await.map_err(Into::into)
}

async fn host_is_cooling_down(db: &PgPool, host_key: &str) -> anyhow::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM source_host_states WHERE host_key=$1 AND next_attempt_at>NOW())"
    ).bind(host_key).fetch_one(db).await.map_err(Into::into)
}

async fn record_host_success(db: &PgPool, host_key: &str) -> anyhow::Result<()> {
    // A request may have started before another creator recorded a new 429.
    // Never let that in-flight success erase an active host-wide cooldown.
    sqlx::query("DELETE FROM source_host_states WHERE host_key=$1 AND next_attempt_at<=NOW()")
        .bind(host_key).execute(db).await?;
    Ok(())
}

async fn record_host_failure(db: &mut sqlx::Transaction<'_, sqlx::Postgres>, host_key: &str, error: &SourceFailure) -> anyhow::Result<()> {
    if matches!(error, SourceFailure::NotFound | SourceFailure::Malformed(_)) {
        return Ok(());
    }
    let (_, _, retry_after) = failure_details(error);
    sqlx::query(
        r#"INSERT INTO source_host_states (host_key,next_attempt_at,consecutive_failures,last_error,updated_at)
        VALUES ($1,NOW()+($2*INTERVAL '1 second'),1,$3,NOW())
        ON CONFLICT (host_key) DO UPDATE SET
        next_attempt_at=GREATEST(source_host_states.next_attempt_at,NOW()+(GREATEST($2,LEAST(60 * (1 << LEAST(source_host_states.consecutive_failures,10)),21600))*INTERVAL '1 second')),
        consecutive_failures=source_host_states.consecutive_failures+1,last_error=$3,updated_at=NOW()"#
    ).bind(host_key).bind(retry_after).bind(format!("{error:?}")).execute(&mut **db).await?;
    Ok(())
}

async fn record_source_success(db: &PgPool, creator_id: Uuid, source_key: &str, host_key: &str, latency_ms: i64) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO creator_source_states (creator_id,source_key,status,last_attempt_at,last_success_at,next_attempt_at,consecutive_failures,total_successes,last_latency_ms,last_http_status,last_error_code,last_error,updated_at)
        VALUES ($1,$2,'healthy',NOW(),NOW(),NULL,0,1,$3,200,NULL,NULL,NOW())
        ON CONFLICT (creator_id,source_key) DO UPDATE SET status='healthy',last_attempt_at=NOW(),last_success_at=NOW(),next_attempt_at=NULL,consecutive_failures=0,total_successes=creator_source_states.total_successes+1,last_latency_ms=EXCLUDED.last_latency_ms,last_http_status=200,last_error_code=NULL,last_error=NULL,updated_at=NOW()"#
    ).bind(creator_id).bind(source_key).bind(latency_ms).execute(db).await?;
    record_host_success(db, host_key).await?;
    Ok(())
}

async fn record_source_failure(db: &PgPool, creator_id: Uuid, source_key: &str, host_key: &str, error: &SourceFailure) -> anyhow::Result<()> {
    let (code, message, delay) = failure_details(error);
    let mut tx = db.begin().await?;
    sqlx::query(
        r#"INSERT INTO creator_source_states (creator_id,source_key,status,last_attempt_at,next_attempt_at,consecutive_failures,total_failures,last_error_code,last_error,updated_at)
        VALUES ($1,$2,'cooldown',NOW(),NOW()+($3*INTERVAL '1 second'),1,1,$4,$5,NOW())
        ON CONFLICT (creator_id,source_key) DO UPDATE SET status='cooldown',last_attempt_at=NOW(),next_attempt_at=NOW()+(GREATEST($3,LEAST(60 * (1 << LEAST(creator_source_states.consecutive_failures,10)),21600))*INTERVAL '1 second'),consecutive_failures=creator_source_states.consecutive_failures+1,total_failures=creator_source_states.total_failures+1,last_error_code=$4,last_error=$5,updated_at=NOW()"#
    ).bind(creator_id).bind(source_key).bind(delay).bind(code).bind(message).execute(&mut *tx).await?;
    record_host_failure(&mut tx, host_key, error).await?;
    tx.commit().await?;
    Ok(())
}

fn should_emit(first_sync: bool, monitoring_started_at: Option<&DateTime<Utc>>, published_at: Option<&DateTime<Utc>>) -> bool {
    if first_sync { return false; }
    let Some(started_at) = monitoring_started_at else { return false; };
    published_at.is_some_and(|published| published >= &(started_at.to_owned() - ChronoDuration::minutes(2)))
}

async fn ingest_profile(db: &PgPool, creator: &DueCreator, profile: PublicProfile, source_key: &str, poll_interval: Duration) -> anyhow::Result<usize> {
    let first_sync = creator.last_polled_at.is_none();
    let mut inserted = 0usize;
    let mut tx = db.begin().await?;
    for video in profile.videos {
        let payload = json!({
            "event": "tiktok.video.published",
            "provider": source_key,
            "creator": {
                "id": creator.id,
                "username": profile.username.as_str(),
                "display_name": profile.display_name.as_deref().or(creator.display_name.as_deref())
            },
            "video": {
                "id": video.id.as_str(),
                "title": video.title.as_deref(),
                "description": video.description.as_deref(),
                "duration": null,
                "cover_image_url": video.cover_image_url.as_deref(),
                "url": video.url.as_str(),
                "embed_url": null,
                "published_at": video.published_at.as_ref()
            }
        });
        let video_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO videos (id,creator_id,platform_video_id,payload,published_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (creator_id,platform_video_id) DO NOTHING RETURNING id"
        ).bind(Uuid::new_v4()).bind(creator.id).bind(&video.id).bind(&payload).bind(video.published_at)
            .fetch_optional(&mut *tx).await?;
        if let Some(video_id) = video_id
            && should_emit(first_sync, creator.monitoring_started_at.as_ref(), video.published_at.as_ref())
        {
            sqlx::query("INSERT INTO deliveries (id,video_id) VALUES ($1,$2)")
                .bind(Uuid::new_v4()).bind(video_id).execute(&mut *tx).await?;
            inserted += 1;
        }
    }
    tx.commit().await?;
    let mut rng = OsRng;
    let jitter_seconds = (rng.next_u32() % 61) as i64;
    let next_poll_seconds = poll_interval.as_secs().max(60) as i64 + jitter_seconds;
    sqlx::query("UPDATE creators SET display_name=COALESCE($2,display_name),last_polled_at=NOW(),last_successful_poll_at=NOW(),next_poll_at=NOW()+($3*INTERVAL '1 second'),poll_lease_until=NULL,poll_failures=0,last_error=NULL WHERE id=$1")
        .bind(creator.id).bind(profile.display_name).bind(next_poll_seconds).execute(db).await?;
    Ok(inserted)
}

async fn poll_one(db: &PgPool, creator: DueCreator, sources: &[SourceAdapter], poll_interval: Duration) -> anyhow::Result<()> {
    let Some(creator) = sqlx::query_as::<_, DueCreator>(
        "UPDATE creators SET poll_lease_until=NOW()+INTERVAL '2 minutes' WHERE id=$1 AND enabled=TRUE AND next_poll_at<=NOW() AND (poll_lease_until IS NULL OR poll_lease_until<NOW()) RETURNING id,username,display_name,last_polled_at,monitoring_started_at,poll_failures"
    ).bind(creator.id).fetch_optional(db).await? else { return Ok(()); };
    let mut last_error = None;
    let mut empty_feed_candidate: Option<(PublicProfile, String)> = None;
    for source in sources {
        let source_key = source.key();
        let host_key = source.host_key();
        if source_is_cooling_down(db, creator.id, &source_key).await? || host_is_cooling_down(db, &host_key).await? { continue; }
        let started = std::time::Instant::now();
        match source.fetch_profile(&creator.username).await {
            Ok(profile) => {
                record_source_success(db, creator.id, &source_key, &host_key, started.elapsed().as_millis() as i64).await?;
                if profile.videos.is_empty() {
                    // An empty feed is valid, but another RSSHub instance may
                    // have fresher cache contents. Keep it as a last-resort baseline.
                    if empty_feed_candidate.is_none() {
                        empty_feed_candidate = Some((profile, source_key));
                    }
                    continue;
                }
                let count = ingest_profile(db, &creator, profile, &source_key, poll_interval).await?;
                tracing::info!(creator=%creator.username, source=%source_key, new_videos=count, first_sync=creator.last_polled_at.is_none(), "public TikTok profile synchronized");
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(creator=%creator.username, source=%source_key, ?error, "public profile source failed; trying next adapter");
                if let Err(write_error) = record_source_failure(db, creator.id, &source_key, &host_key, &error).await {
                    tracing::error!(%write_error, creator=%creator.username, source=%source_key, "could not persist source cooldown; continuing with fallback adapters");
                }
                last_error = Some(format!("{}: {:?}", source_key, error));
            }
        }
    }
    if let Some((profile, source_key)) = empty_feed_candidate {
        let count = ingest_profile(db, &creator, profile, &source_key, poll_interval).await?;
        tracing::info!(creator=%creator.username, source=%source_key, new_videos=count, "public profile synchronized from an empty fallback feed");
        return Ok(());
    }
    let message = last_error.unwrap_or_else(|| "all configured sources are in cooldown".to_owned());
    sqlx::query("UPDATE creators SET poll_lease_until=NULL,poll_failures=poll_failures+1,next_poll_at=NOW()+($2*INTERVAL '1 second'),last_error=$3 WHERE id=$1")
        .bind(creator.id).bind(retry_seconds(creator.poll_failures)).bind(message.chars().take(1000).collect::<String>()).execute(db).await?;
    Ok(())
}

pub async fn run(db: PgPool, sources: Vec<SourceAdapter>, poll_interval: Duration) {
    if sources.is_empty() {
        tracing::error!("public profile monitor has no acquisition sources; creator monitoring is disabled");
        return;
    }
    let sources = Arc::new(sources);
    loop {
        let due = sqlx::query_as::<_, DueCreator>(
            "SELECT id,username,display_name,last_polled_at,monitoring_started_at,poll_failures FROM creators WHERE enabled=TRUE AND next_poll_at<=NOW() AND (poll_lease_until IS NULL OR poll_lease_until<NOW()) ORDER BY next_poll_at,created_at LIMIT 32"
        ).fetch_all(&db).await;
        match due {
            Ok(creators) => {
                let mut tasks = JoinSet::new();
                for creator in creators {
                    while tasks.len() >= 8 {
                        if let Some(Err(error)) = tasks.join_next().await {
                            tracing::error!(%error, "public profile polling task failed");
                        }
                    }
                    let task_db = db.clone();
                    let task_sources = sources.clone();
                    tasks.spawn(async move {
                        poll_one(&task_db, creator, task_sources.as_slice(), poll_interval).await
                    });
                }
                while let Some(result) = tasks.join_next().await {
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::error!(%error, "public profile polling failed"),
                        Err(error) => tracing::error!(%error, "public profile polling task failed"),
                    }
                }
            }
            Err(error) => tracing::error!(%error, "could not load public profiles due for polling"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sync_and_old_feed_items_never_emit() {
        let now = Utc::now();
        assert!(!should_emit(true, None, Some(&now)));
        assert!(!should_emit(false, Some(&(now - ChronoDuration::minutes(10))), Some(&(now - ChronoDuration::minutes(20)))));
        // A feed item discovered late remains eligible when it was published after monitoring began.
        assert!(should_emit(false, Some(&(now - ChronoDuration::minutes(20))), Some(&(now - ChronoDuration::minutes(18)))));
        assert!(!should_emit(false, Some(&now), None));
        assert!(should_emit(false, Some(&(now - ChronoDuration::seconds(30))), Some(&now)));
    }

    #[test]
    fn rate_limit_retry_after_is_respected() {
        let error = SourceFailure::RateLimited {
            retry_after: Some(Duration::from_secs(240)),
        };
        let (code, _, delay) = failure_details(&error);
        assert_eq!(code, "rate_limited");
        assert_eq!(delay, 240);
        let long = SourceFailure::RateLimited { retry_after: Some(Duration::from_secs(86_400)) };
        assert_eq!(failure_details(&long).2, 86_400);
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_seconds(0), 60);
        assert_eq!(retry_seconds(2), 240);
        assert_eq!(retry_seconds(20), 3600);
    }
}