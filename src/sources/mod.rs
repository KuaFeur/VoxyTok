use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicVideo {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub url: String,
    pub cover_image_url: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicProfile {
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub videos: Vec<PublicVideo>,
}

#[derive(Debug, Clone)]
pub enum SourceFailure {
    RateLimited { retry_after: Option<Duration> },
    TemporarilyUnavailable(String),
    NotFound,
    PrivateOrUnavailable,
    Challenge,
    Malformed(String),
}

pub trait ProfileSource: Send + Sync {
    fn key(&self) -> &'static str;
    async fn fetch_profile(&self, username: &str) -> std::result::Result<PublicProfile, SourceFailure>;
}

pub struct TikTokEmbedSource {
    client: Client,
}

impl TikTokEmbedSource {
    pub fn new(client: Client) -> Self { Self { client } }

    fn parse(&self, username: &str, html: &str) -> Result<PublicProfile> {
        let marker = "id=\"__FRONTITY_CONNECT_STATE__\"";
        let marker_pos = html.find(marker).ok_or_else(|| anyhow!("TikTok embed state script is missing"))?;
        let script_start = html[marker_pos..].find('>').map(|n| marker_pos + n + 1)
            .ok_or_else(|| anyhow!("TikTok embed state script has no opening delimiter"))?;
        let script_end = html[script_start..].find("</script>").map(|n| script_start + n)
            .ok_or_else(|| anyhow!("TikTok embed state script is not closed"))?;
        let state: Value = serde_json::from_str(html[script_start..script_end].trim())
            .context("TikTok embed state is not valid JSON")?;
        let (user, videos) = find_profile_payload(&state)
            .ok_or_else(|| anyhow!("TikTok embed state does not contain a profile video list"))?;

        let canonical_username = user.get("uniqueId").and_then(Value::as_str)
            .unwrap_or(username).trim_start_matches('@').to_owned();
        let display_name = user.get("nickname").and_then(Value::as_str).map(str::to_owned);
        let avatar_url = user.get("avatarThumbUrl").and_then(Value::as_str).map(str::to_owned);
        let mut normalized = Vec::new();
        for video in videos.as_array().context("TikTok videoList is not an array")? {
            let Some(id) = video.get("id").and_then(Value::as_str) else { continue; };
            if id.is_empty() { continue; }
            let title = video.get("desc").and_then(Value::as_str).map(str::to_owned);
            let timestamp = video.get("createTime").and_then(Value::as_i64)
                .or_else(|| id.parse::<u64>().ok().map(|value| (value >> 32) as i64));
            let published_at = timestamp.and_then(|value| DateTime::from_timestamp(value, 0));
            normalized.push(PublicVideo {
                id: id.to_owned(),
                title: title.clone(),
                description: title,
                url: format!("https://www.tiktok.com/@{canonical_username}/video/{id}"),
                cover_image_url: video.get("coverUrl").and_then(Value::as_str).map(str::to_owned),
                published_at,
            });
        }
        Ok(PublicProfile { username: canonical_username, display_name, avatar_url, videos: normalized })
    }
}

impl ProfileSource for TikTokEmbedSource {
    fn key(&self) -> &'static str { "tiktok_embed" }

    async fn fetch_profile(&self, username: &str) -> std::result::Result<PublicProfile, SourceFailure> {
        let handle = username.trim_start_matches('@');
        let url = format!("https://www.tiktok.com/embed/@{handle}");
        let response = self.client.get(url).send().await
            .map_err(|error| SourceFailure::TemporarilyUnavailable(error.to_string()))?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND { return Err(SourceFailure::NotFound); }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response.headers().get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs);
            return Err(SourceFailure::RateLimited { retry_after });
        }
        if status == StatusCode::FORBIDDEN {
            return Err(SourceFailure::Challenge);
        }
        if !status.is_success() {
            return Err(SourceFailure::TemporarilyUnavailable(format!("TikTok returned HTTP {status}")));
        }
        let html = response.text().await
            .map_err(|error| SourceFailure::TemporarilyUnavailable(error.to_string()))?;
        if html.contains("_wafchallengeid") || html.contains("captcha") && !html.contains("__FRONTITY_CONNECT_STATE__") {
            return Err(SourceFailure::Challenge);
        }
        self.parse(handle, &html).map_err(|error| SourceFailure::Malformed(error.to_string()))
    }
}

pub struct RssProfileSource {
    client: Client,
    base_url: Url,
}

impl RssProfileSource {
    pub fn new(client: Client, base_url: Url) -> Self { Self { client, base_url } }

    fn feed_url(&self, username: &str) -> Result<Url> {
        let mut url = self.base_url.clone();
        let base = url.path().trim_end_matches('/');
        url.set_path(&format!("{base}/tiktok/user/@{}", username.trim_start_matches('@')));
        Ok(url)
    }
}

impl ProfileSource for RssProfileSource {
    fn key(&self) -> &'static str { "rsshub" }

    async fn fetch_profile(&self, username: &str) -> std::result::Result<PublicProfile, SourceFailure> {
        let url = self.feed_url(username).map_err(|e| SourceFailure::Malformed(e.to_string()))?;
        let response = self.client.get(url).send().await
            .map_err(|error| SourceFailure::TemporarilyUnavailable(error.to_string()))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response.headers().get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(SourceFailure::RateLimited { retry_after });
        }
        if status == StatusCode::NOT_FOUND { return Err(SourceFailure::NotFound); }
        if !status.is_success() {
            return Err(SourceFailure::TemporarilyUnavailable(format!("RSS source returned HTTP {status}")));
        }
        let xml = response.text().await
            .map_err(|error| SourceFailure::TemporarilyUnavailable(error.to_string()))?;
        parse_feed(username, &xml).map_err(|error| SourceFailure::Malformed(error.to_string()))
    }
}

fn find_profile_payload(value: &Value) -> Option<(&Value, &Value)> {
    if let Some(object) = value.as_object() {
        if let (Some(user), Some(videos)) = (object.get("userInfo"), object.get("videoList")) {
            if user.is_object() && videos.is_array() { return Some((user, videos)); }
        }
        for child in object.values() {
            if let Some(found) = find_profile_payload(child) { return Some(found); }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            if let Some(found) = find_profile_payload(child) { return Some(found); }
        }
    }
    None
}

fn parse_feed(username: &str, xml: &str) -> Result<PublicProfile> {
    let entries = if xml.contains("<entry") { split_elements(xml, "entry") } else { split_elements(xml, "item") };
    if entries.is_empty() { bail!("RSS/Atom document contains no entries"); }
    let mut videos = Vec::new();
    for entry in entries {
        let id = tag_value(entry, "guid").or_else(|| tag_value(entry, "id"))
            .or_else(|| tag_value(entry, "link"));
        let Some(id) = id else { continue; };
        let link = tag_value(entry, "link").unwrap_or_else(|| id.clone());
        let video_id = link.rsplit('/').next().unwrap_or(&id).split('?').next().unwrap_or(&id).to_owned();
        if video_id.is_empty() { continue; }
        let title = tag_value(entry, "title");
        let published = tag_value(entry, "pubDate").or_else(|| tag_value(entry, "published"))
            .or_else(|| tag_value(entry, "updated"));
        let published_at = published.and_then(|value| DateTime::parse_from_rfc2822(&value).ok()
            .map(|v| v.with_timezone(&Utc))
            .or_else(|| DateTime::parse_from_rfc3339(&value).ok().map(|v| v.with_timezone(&Utc))));
        videos.push(PublicVideo {
            id: video_id,
            title: title.clone(),
            description: tag_value(entry, "description").or(title),
            url: link,
            cover_image_url: None,
            published_at,
        });
    }
    Ok(PublicProfile { username: username.trim_start_matches('@').to_owned(), display_name: None, avatar_url: None, videos })
}

fn split_elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut result = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after_open = &rest[start..];
        let Some(end) = after_open.find(&close) else { break; };
        result.push(&after_open[..end + close.len()]);
        rest = &after_open[end + close.len()..];
    }
    result
}

fn tag_value(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let start = xml.find(&open)?;
    let content_start = xml[start..].find('>')? + start + 1;
    let close = format!("</{name}>");
    let end = xml[content_start..].find(&close)? + content_start;
    let mut value = xml[content_start..end].trim().to_owned();
    if value.starts_with("<![CDATA[") && value.ends_with("]]>") {
        value = value[9..value.len()-3].to_owned();
    }
    Some(value.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
        .replace("&quot;", "\"").replace("&apos;", "'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tiktok_embedded_profile_state() {
        let html = r#"<script id="__FRONTITY_CONNECT_STATE__" type="application/json">{"source":{"data":{"/embed/@demo":{"userInfo":{"uniqueId":"demo","nickname":"Demo","avatarThumbUrl":"https://img"},"videoList":[{"id":"7350000000000000000","desc":"hello","coverUrl":"https://cover","createTime":1710000000}]}}}}</script>"#;
        let client = Client::builder().build().unwrap();
        let source = TikTokEmbedSource::new(client);
        let profile = source.parse("demo", html).unwrap();
        assert_eq!(profile.username, "demo");
        assert_eq!(profile.videos.len(), 1);
        assert_eq!(profile.videos[0].id, "7350000000000000000");
        assert_eq!(profile.videos[0].title.as_deref(), Some("hello"));
    }

    #[test]
    fn parses_rss_and_atom_entries() {
        let rss = r#"<rss><channel><item><title>One</title><guid>https://www.tiktok.com/@demo/video/123</guid><link>https://www.tiktok.com/@demo/video/123</link><pubDate>Wed, 18 Nov 2026 00:00:00 +0000</pubDate></item></channel></rss>"#;
        let profile = parse_feed("demo", rss).unwrap();
        assert_eq!(profile.videos[0].id, "123");
        let atom = r#"<feed><entry><title>Two</title><id>https://www.tiktok.com/@demo/video/456</id><link href="https://www.tiktok.com/@demo/video/456"/><updated>2026-11-18T00:00:00Z</updated></entry></feed>"#;
        let profile = parse_feed("demo", atom).unwrap();
        assert_eq!(profile.videos[0].id, "456");
    }
}
