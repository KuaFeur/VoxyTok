use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
use crate::sources::{ProfileSource, PublicProfile, SourceFailure};

#[derive(Clone, FromRow)]
struct DueCreator {
    id: Uuid,
    username: String,
    display_name: Option<String>,
    last_polled_at: Option<DateTime<Utc>>,
}

fn retry_seconds(failures: i32) -> i64 {
    60_i64.saturating_mul(1_i64 << failures.clamp(0, 6)).min(3600)
}

fn failure_details(error: &SourceFailure) -> (String, String, i64) {
    match error {
        SourceFailure::RateLimited { retry_after } => (
            "rate_limited".to_owned(), "source returned HTTP 429".to_owned(),
            retry_after.map(|v| v.as_secs() as i64).unwrap_or(900).clamp(60, 21_600),
        ),
        SourceFailure::NotFound => ("not_found".to_owned(), "public profile was not found".to_owned(), 3600),
        SourceFailure::PrivateOrUnavailable => ("unavailable".to_owned(), "profile is private or unavailable".to_owned(), 1800),
        SourceFailure::Challenge => ("challenge".to_owned(), "source returned an access challenge".to_owned(), 3600),
        SourceFailure::Malformed(message) => ("malformed".to_owned(), message.chars().take(500).collect(), 900),
        SourceFailure::TemporarilyUnavailable(message) => ("unavailable".to_owned(), message.chars().take(500).collect(), 300),
    }
}

async fn source_is_cooling_down(db: &PgPool, creator_id: Uuid, source_key: &str) -> anyhow::Result<bool> {
    let next: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_attempt_at FROM creator_source_states WHERE creator_id=$1 AND source_key=$2 AND status='cooldown'"
    ).bind(creator_id).bind(source_key).fetch_optional(db).await?;
    Ok(next.is_some_and(|at| at > Utc::now()))
}

async fn record_source_success(db: &PgPool, creator_id: Uuid, source_key: &str, latency_ms: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO creator_source_states (creator_id,source_key,status,last_attempt_at,last_success_at,next_attempt_at,consecutive_failures,total_successes,last_latency_ms,last_http_status,last_error_code,last_error,updated_at)
        " VALUES ($1,$2,'healthy',NOW(),NOW(),NULL,0,1,$3,200,NULL,NULL,NOW())
        " ON CONFLICT (creator_id,source_key) DO UPDATE SET status='healthy',last_attempt_at=NOW(),last_success_at=NOW(),next_attempt_at=NULL,consecutive_failures=0,total_successes=creator_source_states.total_successes+1,last_latency_ms=EXCLUDED.last_latency_ms,last_http_status=200,last_error_code=NULL,last_error=NULL,updated_at=NOW()"
    ).bind(creator_id).bind(source_key).bind(latency_ms).execute(db).await?;
    Ok(())
}

async fn record_source_failure(db: &PgPool, creator_id: Uuid, source_key: &str, error: &SourceFailure) -> anyhow::Result<()> {
    let (code, message, delay) = failure_details(error);
    sqlx::query(
        "INSERT INTO creator_source_states (creator_id,source_key,status,last_attempt_at,next_attempt_at,consecutive_failures,total_failures,last_error_code,last_error,updated_at)
        " VALUES ($1,$2,'cooldown',NOW(),NOW()+($3*INTERVAL '1 second'),1,1,$4,$5,NOW())
        " ON CONFLICT (creator_id,source_key) DO UPDATE SET status='cooldown',last_attempt_at=NOW(),next_attempt_at=NOW()+($3*INTERVAL '1 second'),consecutive_failures=creator_source_states.consecutive_failures+1,total_failures=creator_source_states.total_failures+1,last_error_code=$4,last_error=$5,updated_at=NOW()"
    ).bind(creator_id).bind(source_key).bind(delay).bind(code).bind(message).execute(db).await?;
    Ok(())
}

async fn ingest_profile(db: &PgPool, creator: &DueCreator, profile: PublicProfile, source_key: &str) -> anyhow::Result<usize> {
    let first_sync = creator.last_polled_at.is_none();
    let mut inserted = 0usize;
    for video in profile.videos {
        let payload = json!({
            "event": "tiktok.video.published",
            "provider": source_key,
            "creator": {
                "id": creator.id,
                "username": profile.username,
                "display_name": profile.display_name.as_deref().or(creator.display_name.as_deref())
            },
            "video": {
                "id": video.id,
                "title": video.title,
                "description": video.description,
                "duration": null,
                "cover_image_url": video.cover_image_url,
                "url": video.url,
                "embed_url": null,
                "published_at": video.published_at
            }
        });
        let mut tx = db.begin().await?;
        let video_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO videos (id,creator_id,platform_video_id,payload,published_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (creator_id,platform_video_id) DO NOTHING RETURNING id"
        ).bind(Uuid::new_v4()).bind(creator.id).bind(&video.id).bind(&payload).bind(video.published_at)
            .fetch_optional(&mut *tx).await?;
        if let Some(video_id) = video_id {
            if !first_sync {
                sqlx::query("INSERT INTO deliveries (id,video_id) VALUES ($1,$2)")
                    .bind(Uuid::new_v4()).bind(video_id).execute(&mut *tx).await?;
                inserted += 1;
            }
        }
        tx.commit().await?;
    }
    sqlx::query("UPDATE creators SET display_name=COALESCE($2,display_name),last_polled_at=NOW(),last_successful_poll_at=NOW(),next_poll_at=NOW()+($3*INTERVAL '1 second'),poll_lease_until=NULL,poll_failures=0,last_error=NULL WHERE id=$1")
        .bind(creator.id).bind(profile.display_name).bind(300_i64).execute(db).await?;
    Ok(inserted)
}

async fn poll_one(db: &PgPool, creator: DueCreator, sources: &[Arc<dyn ProfileSource>], poll_interval: Duration) -> anyhow::Result<()> {
    let lease = sqlx::query_scalar::<_, Uuid>(
        "UPDATE creators SET poll_lease_until=NOW()+INTERVAL '2 minutes' WHERE id=$1 AND (poll_lease_until IS NULL OR poll_lease_until<NOW()) RETURNING id"
    ).bind(creator.id).fetch_optional(db).await?;
    if lease.is_none() { return Ok(()); }
    let mut last_error = None;
    for source in sources {
        if source_is_cooling_down(db, creator.id, source.key()).await? { continue; }
        let started = std::time::Instant::now();
        match source.fetch_profile(&creator.username).await {
            Ok(profile) => {
                record_source_success(db, creator.id, source.key(), started.elapsed().as_millis() as i64).await?;
                let count = ingest_profile(db, &creator, profile, source.key()).await?;
                tracing::info!(creator=%creator.username, source=source.key(), new_videos=count, first_sync=creator.last_polled_at.is_none(), "public TikTok profile synchronized");
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(creator=%creator.username, source=source.key(), ?error, "public profile source failed; trying next adapter");
                record_source_failure(db, creator.id, source.key(), &error).await?;
                last_error = Some(format!("{}: {:?}", source.key(), error));
            }
        }
    }
    let message = last_error.unwrap_or_else(|| "all configured sources are in cooldown".to_owned());
    sqlx::query("UPDATE creators SET poll_lease_until=NULL,poll_failures=poll_failures+1,next_poll_at=NOW()+($2*INTERVAL '1 second'),last_error=$3 WHERE id=$1")
        .bind(creator.id).bind(retry_seconds(creator.last_polled_at.map(|_| 1).unwrap_or(0))).bind(message.chars().take(1000).collect::<String>()).execute(db).await?;
    let _ = poll_interval;
    Ok(())
}

pub async fn run(db: PgPool, sources: Vec<Arc<dyn ProfileSource>>, poll_interval: Duration) {
    if sources.is_empty() {
        tracing::error!("public profile monitor has no acquisition sources; creator monitoring is disabled");
        return;
    }
    loop {
        let due = sqlx::query_as::<_, DueCreator>(
            "SELECT id,username,display_name,last_polled_at FROM creators WHERE enabled=TRUE AND tiktok_open_id IS NULL AND next_poll_at<=NOW() AND (poll_lease_until IS NULL OR poll_lease_until<NOW()) ORDER BY next_poll_at,created_at LIMIT 10"
        ).fetch_all(&db).await;
        match due {
            Ok(creators) => {
                for creator in creators {
                    if let Err(error) = poll_one(&db, creator, &sources, poll_interval).await {
                        tracing::error!(%error, "public profile polling failed");
                    }
                }
            }
            Err(error) => tracing::error!(%error, "could not load public profiles due for polling"),
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_seconds(0), 60);
        assert_eq!(retry_seconds(2), 240);
        assert_eq!(retry_seconds(20), 3600);
    }
}