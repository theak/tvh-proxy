//! Thin client for the tvheadend HTTP API: channel list, EPG, and stream URLs.

use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Channel {
    pub uuid: String,
    pub name: String,
    pub number: String,
    /// URL the browser loads: tvheadend's absolute icon URL, or our `/logo/{uuid}` proxy.
    pub icon: Option<String>,
    /// Relative tvheadend icon path (e.g. `imagecache/12`), fetched via `/logo/{uuid}`.
    #[serde(skip)]
    pub icon_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Event {
    pub channel: String,
    pub start: i64,
    pub stop: i64,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<&'static str>,
}

/// Sort key for "5.3" / "11" / "" style channel numbers: numeric major/minor, blanks last.
pub fn channel_sort_key(number: &str) -> (u32, u32, u32) {
    let mut parts = number.trim().split('.');
    let major = parts.next().and_then(|p| p.trim().parse().ok());
    let minor = parts
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(0);
    match major {
        Some(m) => (0, m, minor),
        None => (1, u32::MAX, 0),
    }
}

/// ETSI EN 300 468 content nibble → a short label for the pill in the UI.
pub fn genre_label(code: u64) -> Option<&'static str> {
    Some(match code >> 4 {
        0x1 => "Movie",
        0x2 => "News",
        0x3 => "Show",
        0x4 => "Sports",
        0x5 => "Kids",
        0x6 => "Music",
        0x7 => "Arts",
        0x8 => "Current Affairs",
        0x9 => "Education",
        0xA => "Leisure",
        _ => return None,
    })
}

fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

pub fn parse_channels(body: &Value) -> Vec<Channel> {
    let mut out: Vec<Channel> = body
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|e| e.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|e| {
            let uuid = s(e, "uuid");
            if uuid.is_empty() {
                return None;
            }
            let raw_icon = {
                let p = s(e, "icon_public_url");
                if p.is_empty() { s(e, "icon") } else { p }
            };
            let (icon, icon_path) =
                if raw_icon.starts_with("http://") || raw_icon.starts_with("https://") {
                    (Some(raw_icon), None)
                } else if raw_icon.is_empty() {
                    (None, None)
                } else {
                    (
                        Some(format!("/logo/{uuid}")),
                        Some(raw_icon.trim_start_matches('/').to_string()),
                    )
                };
            Some(Channel {
                name: s(e, "name"),
                number: s(e, "number"),
                uuid,
                icon,
                icon_path,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        channel_sort_key(&a.number)
            .cmp(&channel_sort_key(&b.number))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

pub fn parse_events(body: &Value) -> Vec<Event> {
    body.get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let start = e.get("start").and_then(Value::as_i64)?;
            let stop = e.get("stop").and_then(Value::as_i64)?;
            let channel = s(e, "channelUuid");
            if channel.is_empty() || stop <= start {
                return None;
            }
            let description = {
                let d = s(e, "description");
                if d.is_empty() { s(e, "summary") } else { d }
            };
            let category = e
                .get("genre")
                .and_then(Value::as_array)
                .and_then(|g| g.iter().filter_map(Value::as_u64).find_map(genre_label));
            Some(Event {
                channel,
                start,
                stop,
                title: s(e, "title"),
                subtitle: s(e, "subtitle"),
                description,
                category,
            })
        })
        .collect()
}

/// The keyword feature: walk keywords in priority order and return the first channel
/// (in channel-number order) whose *currently airing* show mentions it, matching
/// case-insensitively across title, episode title, description and category.
pub fn keyword_channel(
    keywords: &[String],
    channels: &[Channel],
    events: &[Event],
    now: i64,
) -> Option<(String, String)> {
    let airing: Vec<&Event> = events
        .iter()
        .filter(|e| e.start <= now && now < e.stop)
        .collect();
    for kw in keywords {
        let kw = kw.to_lowercase();
        for ch in channels {
            let hit = airing.iter().any(|e| {
                e.channel == ch.uuid
                    && [
                        e.title.as_str(),
                        e.subtitle.as_str(),
                        e.description.as_str(),
                        e.category.unwrap_or(""),
                    ]
                    .iter()
                    .any(|f| f.to_lowercase().contains(&kw))
            });
            if hit {
                return Some((ch.uuid.clone(), kw));
            }
        }
    }
    None
}

#[derive(Clone)]
pub struct Tvh {
    pub base: String,
    pub username: String,
    pub password: String,
    pub client: reqwest::Client,
}

/// Parse a `WWW-Authenticate: Digest k="v", k2=v2` challenge into key/value pairs.
pub fn parse_digest_challenge(header: &str) -> Option<std::collections::HashMap<String, String>> {
    let rest = header.trim().strip_prefix("Digest")?.trim();
    let mut out = std::collections::HashMap::new();
    let mut s = rest;
    while !s.is_empty() {
        let (key, after) = s.split_once('=')?;
        let key = key
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_ascii_lowercase();
        let after = after.trim_start();
        let (value, remaining) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (&q[..end], &q[end + 1..])
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim(), &after[end..])
        };
        out.insert(key, value.to_string());
        s = remaining.trim_start().trim_start_matches(',').trim_start();
    }
    Some(out)
}

fn md5_hex(s: &str) -> String {
    use md5::{Digest, Md5};
    hex::encode(Md5::digest(s.as_bytes()))
}

/// RFC 2617 Digest `Authorization` header (qop=auth, or legacy no-qop). tvheadend
/// uses Digest by default, so Basic credentials are rejected.
pub fn digest_authorization(
    user: &str,
    pass: &str,
    method: &str,
    uri: &str,
    challenge: &std::collections::HashMap<String, String>,
    cnonce: &str,
) -> String {
    let get = |k: &str| challenge.get(k).map(String::as_str).unwrap_or("");
    let (realm, nonce, opaque) = (get("realm"), get("nonce"), get("opaque"));
    let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let qop_auth = get("qop").split(',').any(|q| q.trim() == "auth");
    let nc = "00000001";
    let response = if qop_auth {
        md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"))
    } else {
        md5_hex(&format!("{ha1}:{nonce}:{ha2}"))
    };
    let mut h = format!(
        r#"Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", response="{response}""#
    );
    if qop_auth {
        h.push_str(&format!(r#", qop=auth, nc={nc}, cnonce="{cnonce}""#));
    }
    if !opaque.is_empty() {
        h.push_str(&format!(r#", opaque="{opaque}""#));
    }
    h
}

impl Tvh {
    /// GET a tvheadend path, answering a Digest challenge when credentials are set
    /// (falling back to Basic if that's what the server asks for).
    async fn send(&self, path: &str) -> reqwest::Result<reqwest::Response> {
        let path = path.trim_start_matches('/');
        let url = format!("{}/{}", self.base, path);
        let resp = self.client.get(&url).send().await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED || self.username.is_empty() {
            return Ok(resp);
        }
        let challenge = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_digest_challenge);
        let retry = self.client.get(&url);
        let retry = match challenge {
            Some(c) => {
                // The `uri` must be the request-target exactly as sent: path + query.
                let prefix = self
                    .base
                    .split_once("://")
                    .map(|(_, r)| r)
                    .unwrap_or(&self.base);
                let base_path = prefix.find('/').map(|i| &prefix[i..]).unwrap_or("");
                let uri = format!("{base_path}/{path}");
                let cnonce = &crate::auth::random_token()[..16];
                retry.header(
                    reqwest::header::AUTHORIZATION,
                    digest_authorization(&self.username, &self.password, "GET", &uri, &c, cnonce),
                )
            }
            None => retry.basic_auth(&self.username, Some(&self.password)),
        };
        retry.send().await
    }

    pub(crate) async fn json(&self, path: &str) -> Result<Value, String> {
        let resp = self
            .send(path)
            .await
            .map_err(|e| format!("Can't reach tvheadend: {e}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err("tvheadend refused access — check the tvheadend username/password, or allow anonymous access from this host.".into());
        }
        if !resp.status().is_success() {
            return Err(format!(
                "tvheadend returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        resp.json()
            .await
            .map_err(|e| format!("Unexpected reply from tvheadend: {e}"))
    }

    pub async fn channels(&self) -> Result<Vec<Channel>, String> {
        Ok(parse_channels(
            &self.json("api/channel/grid?limit=5000").await?,
        ))
    }

    pub async fn events(&self) -> Result<Vec<Event>, String> {
        Ok(parse_events(
            &self.json("api/epg/events/grid?limit=20000").await?,
        ))
    }

    pub async fn icon(&self, path: &str) -> Option<(String, Vec<u8>)> {
        let resp = self.send(path).await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/png")
            .to_string();
        if !ctype.starts_with("image/") {
            return None;
        }
        Some((ctype, resp.bytes().await.ok()?.to_vec()))
    }

    /// The raw broadcast ("pass" profile) for ffmpeg. Credentials go in the URL
    /// because ffmpeg handles both basic and digest auth from there.
    pub fn stream_url(&self, channel_uuid: &str) -> String {
        let base = if self.username.is_empty() {
            self.base.clone()
        } else {
            let (scheme, rest) = self.base.split_once("://").unwrap_or(("http", &self.base));
            format!(
                "{scheme}://{}:{}@{rest}",
                pct(&self.username),
                pct(&self.password)
            )
        };
        format!("{base}/stream/channel/{channel_uuid}?profile=pass")
    }
}

/// Minimal percent-encoding for userinfo in a URL.
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
