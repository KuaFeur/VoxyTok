use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    DateTime::parse_from_rfc2822(value).ok().and_then(|date| {
        let seconds = (date.with_timezone(&Utc) - Utc::now()).num_seconds();
        (seconds > 0).then_some(Duration::from_secs(seconds as u64))
    })
}

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
    Challenge,
    Malformed(String),
}

#[derive(Clone)]
pub struct TikTokPublicProfileSource {
    client: Client,
}

impl TikTokPublicProfileSource {
    pub fn new(client: Client) -> Self { Self { client } }

    fn parse(&self, username: &str, html: &str) -> Result<PublicProfile> {
        let marker = r#"id="__FRONTITY_CONNECT_STATE__""#;
        let marker_pos = html.find(marker).ok_or_else(|| anyhow!("TikTok embed state script is missing"))?;
        let script_start = html[marker_pos..].find('>').map(|n| marker_pos + n + 1)
            .ok_or_else(|| anyhow!("TikTok embed state script has no opening delimiter"))?;
        let script_end = html[script_start..].find("</script>").map(|n| script_start + n)
            .ok_or_else(|| anyhow!("TikTok embed state script is not closed"))?;
        let state: Value = serde_json::from_str(html[script_start..script_end].trim())
            .context("TikTok embed state is not valid JSON")?;
        let user_info = find_value(&state, "userInfo")
            .ok_or_else(|| anyhow!("TikTok public profile response has no userInfo"))?;
        let user = user_info.get("user").filter(|value| value.is_object()).unwrap_or(user_info);
        let canonical_username = user.get("uniqueId").and_then(Value::as_str)
            .unwrap_or(username).trim_start_matches('@').to_owned();
        if user.get("privateAccount").and_then(Value::as_bool).unwrap_or(false)
            || user.get("isPrivate").and_then(Value::as_bool).unwrap_or(false) {
            bail!("TikTok profile is private");
        }
        let display_name = user.get("nickname").and_then(Value::as_str).map(str::to_owned);
        let avatar_url = user.get("avatarThumbUrl").or_else(|| user.get("avatarLarger"))
            .or_else(|| user.get("avatarMedium")).and_then(Value::as_str).map(str::to_owned);
        let videos = find_value(&state, "videoList")
            .or_else(|| find_value(&state, "itemList"))
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("TikTok public profile response has no video list"))?;
        let mut normalized = Vec::new();
        for video in videos {
            let Some(id) = video.get("id").and_then(Value::as_str) else { continue; };
            if id.is_empty() { continue; }
            let title = video.get("desc").and_then(Value::as_str).map(str::to_owned);
            let video_meta = video.get("video").unwrap_or(video);
            let timestamp = video.get("createTime").and_then(Value::as_i64)
                .or_else(|| id.parse::<u64>().ok().map(|value| (value >> 32) as i64));
            let published_at = timestamp.and_then(|value| DateTime::from_timestamp(value, 0));
            normalized.push(PublicVideo {
                id: id.to_owned(),
                title: title.clone(),
                description: title,
                url: format!("https://www.tiktok.com/@{canonical_username}/video/{id}"),
                cover_image_url: video_meta.get("coverUrl").or_else(|| video_meta.get("cover")).or_else(|| video_meta.get("originCover"))
                    .and_then(Value::as_str).map(str::to_owned),
                published_at,
            });
        }
        Ok(PublicProfile { username: canonical_username, display_name, avatar_url, videos: normalized })
    }
}

impl TikTokPublicProfileSource {
    pub async fn fetch_profile(&self, username: &str) -> std::result::Result<PublicProfile, SourceFailure> {
        let handle = username.trim_start_matches('@');
        let url = format!("https://www.tiktok.com/embed/@{handle}");
        let response = self.client.get(url).send().await
            .map_err(|error| SourceFailure::TemporarilyUnavailable(error.to_string()))?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND { return Err(SourceFailure::NotFound); }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response.headers().get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()).and_then(|v| parse_retry_after(Some(v)));
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
        self.parse_response_body(handle, &html)
            .map_err(|error| SourceFailure::Malformed(error.to_string()))
    }

    fn parse_response_body(&self, username: &str, html: &str) -> Result<PublicProfile> {
        let lower_html = html.to_ascii_lowercase();
        if html.contains("_wafchallengeid") || (lower_html.contains("captcha") && !html.contains("__FRONTITY_CONNECT_STATE__")) {
            bail!("TikTok returned an access challenge");
        }
        self.parse(username, html)
    }
}

#[derive(Clone)]
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

impl RssProfileSource {
    pub async fn fetch_profile(&self, username: &str) -> std::result::Result<PublicProfile, SourceFailure> {
        let url = self.feed_url(username).map_err(|e| SourceFailure::Malformed(e.to_string()))?;
        let response = self.client.get(url).send().await
            .map_err(|error| SourceFailure::TemporarilyUnavailable(error.to_string()))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response.headers().get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()).and_then(|v| parse_retry_after(Some(v)));
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

#[derive(Clone)]
pub enum SourceAdapter {
    TikTokPublicProfile(TikTokPublicProfileSource),
    RssHub(RssProfileSource),
}

impl SourceAdapter {
    pub fn key(&self) -> String {
        match self {
            Self::TikTokPublicProfile(_) => "tiktok_public_profile".to_owned(),
            Self::RssHub(source) => format!("rsshub:{}", source.base_url.host_str().unwrap_or("unknown")),
        }
    }

    pub fn host_key(&self) -> String {
        match self {
            Self::TikTokPublicProfile(_) => "tiktok.com".to_owned(),
            Self::RssHub(source) => format!("rsshub:{}", source.base_url.host_str().unwrap_or("unknown")),
        }
    }

    pub async fn fetch_profile(&self, username: &str) -> std::result::Result<PublicProfile, SourceFailure> {
        match self {
            Self::TikTokPublicProfile(source) => source.fetch_profile(username).await,
            Self::RssHub(source) => source.fetch_profile(username).await,
        }
    }
}

fn find_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(object) = value.as_object() {
        if let Some(found) = object.get(key) { return Some(found); }
        for child in object.values() {
            if let Some(found) = find_value(child, key) { return Some(found); }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            if let Some(found) = find_value(child, key) { return Some(found); }
        }
    }
    None
}

fn parse_feed(username: &str, xml: &str) -> Result<PublicProfile> {
    let root = validate_xml_document(xml)?;
    let is_atom = root == "feed";
    if root != "rss" && !is_atom {
        bail!("response root is neither RSS nor Atom");
    }
    if is_atom && !root_declares_atom_namespace(xml)? {
        bail!("Atom feed root does not declare the Atom namespace");
    }
    let entries = if is_atom { split_elements(xml, "entry") } else { split_elements(xml, "item") };
    let mut videos = Vec::new();
    for entry in entries {
        let id = tag_value(entry, "guid").or_else(|| tag_value(entry, "id"))
            .or_else(|| tag_value(entry, "link"));
        let Some(id) = id else { continue; };
        let link = tag_value(entry, "link").or_else(|| attribute_value(entry, "link", "href")).unwrap_or_else(|| id.clone());
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

fn root_declares_atom_namespace(xml: &str) -> Result<bool> {
    let start = xml.find('<').ok_or_else(|| anyhow!("XML document has no root element"))?;
    let rest = &xml[start..];
    let end = rest.find('>').ok_or_else(|| anyhow!("unterminated XML root element"))?;
    let header = &rest[1..end];
    let qualified = header.split_whitespace().next().unwrap_or_default();
    let namespace_attribute = if let Some((prefix, _)) = qualified.split_once(':') {
        format!("xmlns:{prefix}=\"http://www.w3.org/2005/Atom\"")
    } else {
        "xmlns=\"http://www.w3.org/2005/Atom\"".to_owned()
    };
    Ok(header.contains(&namespace_attribute))
}

// Validate well-formed element nesting and return the document root's local name.
// Prefixes are ignored for RSS/Atom vocabulary matching, as required by XML namespaces.
fn validate_xml_document(xml: &str) -> Result<String> {
    let mut stack: Vec<String> = Vec::new();
    let mut root: Option<String> = None;
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        rest = &rest[open..];
        if rest.starts_with("<!--") {
            let end = rest.find("-->").ok_or_else(|| anyhow!("unterminated XML comment"))?;
            rest = &rest[end + 3..];
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").ok_or_else(|| anyhow!("unterminated CDATA section"))?;
            rest = &rest[end + 3..];
            continue;
        }
        if rest.starts_with("<?") {
            let end = rest.find("?>").ok_or_else(|| anyhow!("unterminated XML processing instruction"))?;
            rest = &rest[end + 2..];
            continue;
        }
        if rest.starts_with("<!") {
            bail!("XML declarations other than comments and CDATA are not supported");
        }
        let close = rest.starts_with("</");
        let name_start = if close { 2 } else { 1 };
        let name_end = rest[name_start..].find(|c: char| c.is_whitespace() || c == '>' || c == '/') 
            .map(|n| name_start + n).ok_or_else(|| anyhow!("malformed XML tag"))?;
        let qualified = &rest[name_start..name_end];
        if qualified.is_empty() { bail!("empty XML tag name"); }
        let local = qualified.rsplit(':').next().unwrap_or(qualified).to_owned();
        let mut quote = None;
        let mut tag_end = None;
        for (relative_index, ch) in rest[name_end..].char_indices() {
            let index = name_end + relative_index;
            if let Some(q) = quote {
                if ch == q { quote = None; }
            } else if ch == '"' || ch == '\'' {
                quote = Some(ch);
            } else if ch == '>' {
                tag_end = Some(index);
                break;
            }
        }
        let tag_end = tag_end.ok_or_else(|| anyhow!("unterminated XML tag"))?;
        let self_closing = !close && rest[..tag_end].trim_end().ends_with('/');
        if close {
            if stack.pop().as_deref() != Some(qualified) {
                bail!("mismatched XML closing tag");
            }
        } else {
            if stack.is_empty() {
                if root.is_some() { bail!("XML document has multiple root elements"); }
                root = Some(local);
            }
            if !self_closing { stack.push(qualified.to_owned()); }
        }
        rest = &rest[tag_end + 1..];
    }
    if !stack.is_empty() { bail!("XML document has unclosed elements"); }
    root.ok_or_else(|| anyhow!("XML document has no root element"))
}

fn split_elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut result = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        let after_start = &rest[start..];
        let Some(name_end) = after_start.find(|c: char| c.is_whitespace() || c == '>' || c == '/') else { break; };
        let qualified = &after_start[1..name_end];
        if qualified.rsplit(':').next() != Some(name) || after_start.starts_with("</") {
            rest = &after_start[1..];
            continue;
        }
        let close_tag = format!("</{qualified}>");
        let Some(end) = after_start.find(&close_tag) else { break; };
        result.push(&after_start[..end + close_tag.len()]);
        rest = &after_start[end + close_tag.len()..];
    }
    result
}

fn find_local_tag(xml: &str, local_name: &str) -> Option<usize> {
    let mut rest = xml;
    let mut offset = 0;
    while let Some(index) = rest.find('<') {
        let candidate = &rest[index..];
        if candidate.starts_with("</") || candidate.starts_with("<!") || candidate.starts_with("<?") {
            offset += index + 1;
            rest = &candidate[1..];
            continue;
        }
        let end = candidate[1..].find(|c: char| c.is_whitespace() || c == '>' || c == '/').map(|n| n + 1)?;
        if candidate[1..end].rsplit(':').next() == Some(local_name) {
            return Some(offset + index);
        }
        offset += index + 1;
        rest = &candidate[1..];
    }
    None
}

fn attribute_value(xml: &str, tag: &str, attribute: &str) -> Option<String> {
    let start = find_local_tag(xml, tag)?;
    let end = xml[start..].find('>')? + start;
    let header = &xml[start..end];
    let marker = format!("{attribute}=\"");
    let value_start = header.find(&marker)? + marker.len();
    let value_end = header[value_start..].find('"')? + value_start;
    Some(header[value_start..value_end].replace("&amp;", "&"))
}
fn tag_value(xml: &str, name: &str) -> Option<String> {
    let start = find_local_tag(xml, name)?;
    let content_start = xml[start..].find('>')? + start + 1;
    let qualified = xml[start + 1..xml[start..].find('>').map(|n| start+n).unwrap_or(start)].split_whitespace().next()?.trim_end_matches('/');
    let close = format!("</{qualified}>");
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
    fn rsshub_instances_have_independent_source_keys_and_correct_route() {
        let client = Client::builder().build().unwrap();
        let first = RssProfileSource::new(client.clone(), Url::parse("https://rsshub.app").unwrap());
        let second = RssProfileSource::new(client, Url::parse("https://rsshub.rssforever.com").unwrap());
        assert_ne!(
            SourceAdapter::RssHub(first.clone()).key(),
            SourceAdapter::RssHub(second).key()
        );
        assert_eq!(first.feed_url("@demo").unwrap().path(), "/tiktok/user/@demo");
    }

    #[test]
    fn parses_tiktok_embedded_profile_state() {
        let html = r#"<script id="__FRONTITY_CONNECT_STATE__" type="application/json">{"source":{"data":{"/embed/@demo":{"userInfo":{"uniqueId":"demo","nickname":"Demo","avatarThumbUrl":"https://img"},"videoList":[{"id":"7350000000000000000","desc":"hello","coverUrl":"https://cover","createTime":1710000000}]}}}}</script>"#;
        let client = Client::builder().build().unwrap();
        let source = TikTokPublicProfileSource::new(client);
        let profile = source.parse("demo", html).unwrap();
        assert_eq!(profile.username, "demo");
        assert_eq!(profile.videos.len(), 1);
        assert_eq!(profile.videos[0].id, "7350000000000000000");
        assert_eq!(profile.videos[0].title.as_deref(), Some("hello"));
    }

    #[test]
    fn private_profile_detection_uses_structured_account_state_only() {
        let client = Client::builder().build().unwrap();
        let source = TikTokPublicProfileSource::new(client);
        let public_html = r#"<script id="__FRONTITY_CONNECT_STATE__" type="application/json">{"userInfo":{"uniqueId":"demo","nickname":"Demo","privateAccount":false},"videoList":[{"id":"123","desc":"this account is private"}]}</script>"#;
        assert!(source.parse_response_body("demo", public_html).is_ok());

        let private_html = r#"<script id="__FRONTITY_CONNECT_STATE__" type="application/json">{"userInfo":{"uniqueId":"demo","privateAccount":true},"videoList":[]}</script>"#;
        assert!(source.parse_response_body("demo", private_html).is_err());
    }

    #[test]
    fn tiktok_host_cooldown_is_shared_without_changing_provider_key() {
        let client = Client::builder().build().unwrap();
        let source = SourceAdapter::TikTokPublicProfile(TikTokPublicProfileSource::new(client));
        assert_eq!(source.key(), "tiktok_public_profile");
        assert_eq!(source.host_key(), "tiktok.com");
    }

    #[test]
    fn accepts_valid_empty_rss_and_atom_feeds() {
        let rss = parse_feed("demo", "<rss><channel></channel></rss>").unwrap();
        assert!(rss.videos.is_empty());
        let atom = parse_feed("demo", r#"<feed xmlns="http://www.w3.org/2005/Atom"></feed>"#).unwrap();
        assert!(atom.videos.is_empty());
        assert!(parse_feed("demo", "<html>no feed</html>").is_err());
        assert!(parse_feed("demo", "<rss><channel></rss>").is_err());
        let prefixed = r#"<atom:feed xmlns:atom="http://www.w3.org/2005/Atom"><atom:entry><atom:id>https://www.tiktok.com/@demo/video/789</atom:id><atom:link href="https://www.tiktok.com/@demo/video/789"/></atom:entry></atom:feed>"#;
        let parsed = parse_feed("demo", prefixed).unwrap();
        assert_eq!(parsed.videos[0].id, "789");
        assert_eq!(parse_retry_after(Some("120")).unwrap(), Duration::from_secs(120));
        let future = (Utc::now() + chrono::Duration::days(1)).to_rfc2822();
        assert!(parse_retry_after(Some(&future)).is_some());
    }

    #[test]
    fn parses_rss_and_atom_entries() {
        let rss = r#"<rss><channel><item><title>One</title><guid>https://www.tiktok.com/@demo/video/123</guid><link>https://www.tiktok.com/@demo/video/123</link><pubDate>Wed, 18 Nov 2026 00:00:00 +0000</pubDate></item></channel></rss>"#;
        let profile = parse_feed("demo", rss).unwrap();
        assert_eq!(profile.videos[0].id, "123");
        let atom = r#"<atom:feed xmlns:atom="http://www.w3.org/2005/Atom"><atom:entry><atom:title>Two</atom:title><atom:id>https://www.tiktok.com/@demo/video/456</atom:id><atom:link href="https://www.tiktok.com/@demo/video/456"/><atom:updated>2026-11-18T00:00:00Z</atom:updated></atom:entry></atom:feed>"#;
        let profile = parse_feed("demo", atom).unwrap();
        assert_eq!(profile.videos[0].id, "456");
    }
}
