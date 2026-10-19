//! HTTP handlers. Every `/api/*` route except status/setup/login, plus `/hls` and
//! `/logo`, requires a valid session cookie. POST bodies are JSON-only, which (with
//! `SameSite=Lax` cookies) also blocks cross-site form submissions.

use crate::stream::{Action, WatchError};
use crate::{AppState, Guide, Shared, auth, config, stream, tvh};
use axum::{
    Json,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INDEX_HTML: &str = include_str!("../templates/index.html");

/// Frontend assets baked into the binary: (path under /static, bytes, content type).
const ASSETS: &[(&str, &[u8], &str)] = &[
    (
        "css/app.css",
        include_bytes!("../static/css/app.css"),
        "text/css; charset=utf-8",
    ),
    (
        "js/app.js",
        include_bytes!("../static/js/app.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/lib.js",
        include_bytes!("../static/js/lib.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "js/hls.min.js",
        include_bytes!("../static/js/hls.min.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "fonts/geist-latin.woff2",
        include_bytes!("../static/fonts/geist-latin.woff2"),
        "font/woff2",
    ),
    (
        "fonts/geist-mono-latin.woff2",
        include_bytes!("../static/fonts/geist-mono-latin.woff2"),
        "font/woff2",
    ),
    (
        "img/icon.svg",
        include_bytes!("../static/img/icon.svg"),
        "image/svg+xml",
    ),
    (
        "img/icon-180.png",
        include_bytes!("../static/img/icon-180.png"),
        "image/png",
    ),
    (
        "img/icon-512.png",
        include_bytes!("../static/img/icon-512.png"),
        "image/png",
    ),
];

/// Cache-busting token for asset URLs: a hash of every embedded asset.
fn asset_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h = sha2::Sha256::new();
        for (_, bytes, _) in ASSETS {
            h.update(bytes);
        }
        hex::encode(h.finalize())[..10].to_string()
    })
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn is_https(state: &AppState, headers: &HeaderMap) -> bool {
    state.cookie_secure
        || headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(auth::token_from_cookie_header)
        .map(str::to_string)
}

fn is_authed(state: &AppState, headers: &HeaderMap) -> bool {
    state.settings.read().unwrap().is_some()
        && session_token(headers).is_some_and(|t| state.sessions.lock().unwrap().is_valid(&t))
}

/// Gate for protected routes.
#[allow(clippy::result_large_err)]
fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    if is_authed(state, headers) {
        Ok(())
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "Please sign in."))
    }
}

/// Who to throttle: the peer address, or the first `X-Forwarded-For` hop when told
/// to trust the reverse proxy in front of us.
fn client_key(state: &AppState, parts: &Parts) -> String {
    if state.trust_proxy
        && let Some(ip) = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    {
        return ip.to_string();
    }
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn with_cookie(mut r: Response, cookie: String) -> Response {
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}

fn throttled(secs: u64) -> Response {
    let mins = secs.div_ceil(60);
    err(
        StatusCode::TOO_MANY_REQUESTS,
        if secs < 90 {
            format!("Too many attempts. Try again in {secs} seconds.")
        } else {
            format!("Too many attempts. Try again in {mins} minutes.")
        },
    )
}

// ---------------------------------------------------------------- pages & assets

pub async fn index() -> Response {
    let html = INDEX_HTML.replace("{{v}}", asset_version());
    no_store(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response())
}

pub async fn manifest() -> Response {
    let v = asset_version();
    let body = json!({
        "name": "TV",
        "short_name": "TV",
        "start_url": "/",
        "display": "standalone",
        "orientation": "any",
        "background_color": "#0B0B0C",
        "theme_color": "#0B0B0C",
        "icons": [
            { "src": format!("/static/img/icon-512.png?v={v}"), "sizes": "512x512", "type": "image/png" },
            { "src": format!("/static/img/icon.svg?v={v}"), "sizes": "any", "type": "image/svg+xml" }
        ]
    });
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        body.to_string(),
    )
        .into_response()
}

pub async fn serve_static(Path(filepath): Path<String>) -> Response {
    match ASSETS.iter().find(|(p, _, _)| *p == filepath) {
        Some((_, bytes, ctype)) => (
            [
                (header::CONTENT_TYPE, *ctype),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            *bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------- setup & auth

pub async fn status(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let configured = state.settings.read().unwrap().is_some();
    no_store(
        Json(json!({ "configured": configured, "authed": is_authed(&state, &headers) }))
            .into_response(),
    )
}

#[derive(Deserialize)]
pub struct SetupBody {
    pub setup_code: String,
    pub tvh_url: String,
    #[serde(default)]
    pub tvh_username: String,
    #[serde(default)]
    pub tvh_password: String,
    pub username: String,
    pub password: String,
    pub confirm: String,
    #[serde(default)]
    pub keywords: String,
    pub max_viewers: u32,
}

pub async fn setup(
    State(state): State<Shared>,
    parts: Parts,
    Json(b): Json<SetupBody>,
) -> Response {
    if state.settings.read().unwrap().is_some() {
        return err(StatusCode::CONFLICT, "Already set up. Sign in instead.");
    }
    let key = format!("setup:{}", client_key(&state, &parts));
    if let Err(secs) = state.limiter.lock().unwrap().check(&key, Instant::now()) {
        return throttled(secs);
    }
    let code_ok = {
        let code = state.setup_code.lock().unwrap();
        code.as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(b.setup_code.trim()))
    };
    if !code_ok {
        state
            .limiter
            .lock()
            .unwrap()
            .record_failure(&key, Instant::now());
        return err(
            StatusCode::FORBIDDEN,
            "That setup code isn't right. It's printed in the container log.",
        );
    }
    let Some(tvh_url) = config::normalize_tvh_url(&b.tvh_url) else {
        return err(
            StatusCode::BAD_REQUEST,
            "Enter tvheadend's address, like http://192.168.0.149:9981",
        );
    };
    if let Err(e) = config::validate_credentials(&b.username, &b.password, &b.confirm) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    let max_viewers = match config::validate_max_viewers(b.max_viewers) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let probe = tvh::Tvh {
        base: tvh_url.clone(),
        username: b.tvh_username.trim().into(),
        password: b.tvh_password.clone(),
        client: state.http.clone(),
    };
    if let Err(e) = probe.channels().await {
        return err(StatusCode::BAD_GATEWAY, e);
    }
    let settings = config::Settings {
        tvh_url,
        tvh_username: b.tvh_username.trim().into(),
        tvh_password: b.tvh_password,
        username: b.username.trim().into(),
        password_hash: config::hash_password(&b.password),
        keywords: config::parse_keywords(&b.keywords),
        max_viewers,
    };
    if let Err(e) = config::save(&state.data_dir, &settings) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't save settings: {e}"),
        );
    }
    *state.settings.write().unwrap() = Some(settings);
    *state.setup_code.lock().unwrap() = None;
    state.hub.lock().unwrap().set_max(max_viewers);
    let token = state.sessions.lock().unwrap().create();
    with_cookie(
        Json(json!({ "ok": true })).into_response(),
        auth::session_cookie(&token, is_https(&state, &parts.headers)),
    )
}

#[derive(Deserialize)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
}

pub async fn login(
    State(state): State<Shared>,
    parts: Parts,
    Json(b): Json<LoginBody>,
) -> Response {
    let Some((user, hash)) = state
        .settings
        .read()
        .unwrap()
        .as_ref()
        .map(|s| (s.username.clone(), s.password_hash.clone()))
    else {
        return err(StatusCode::CONFLICT, "Finish setup first.");
    };
    let key = client_key(&state, &parts);
    if let Err(secs) = state.limiter.lock().unwrap().check(&key, Instant::now()) {
        return throttled(secs);
    }
    // Always run the (slow) hash check so a wrong username takes as long as a wrong password.
    let password = b.password;
    let hash_ok = tokio::task::spawn_blocking(move || config::verify_password(&password, &hash))
        .await
        .unwrap_or(false);
    if !(hash_ok && b.username.trim() == user) {
        state
            .limiter
            .lock()
            .unwrap()
            .record_failure(&key, Instant::now());
        tokio::time::sleep(Duration::from_millis(400)).await;
        return err(StatusCode::UNAUTHORIZED, "Wrong username or password.");
    }
    state.limiter.lock().unwrap().record_success(&key);
    let token = state.sessions.lock().unwrap().create();
    with_cookie(
        Json(json!({ "ok": true })).into_response(),
        auth::session_cookie(&token, is_https(&state, &parts.headers)),
    )
}

pub async fn logout(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(t) = session_token(&headers) {
        state.sessions.lock().unwrap().revoke(&t);
    }
    with_cookie(
        Json(json!({ "ok": true })).into_response(),
        auth::clear_cookie(is_https(&state, &headers)),
    )
}

// ---------------------------------------------------------------- settings

pub async fn get_settings(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    let s = state
        .settings
        .read()
        .unwrap()
        .clone()
        .expect("authed implies configured");
    no_store(
        Json(json!({
            "tvh_url": s.tvh_url,
            "tvh_username": s.tvh_username,
            "has_tvh_password": !s.tvh_password.is_empty(),
            "username": s.username,
            "keywords": s.keywords.join(", "),
            "max_viewers": s.max_viewers,
            "max_viewers_limit": config::MAX_VIEWERS_LIMIT,
        }))
        .into_response(),
    )
}

#[derive(Deserialize)]
pub struct SettingsBody {
    pub tvh_url: String,
    #[serde(default)]
    pub tvh_username: String,
    /// `None` keeps the stored tvheadend password.
    pub tvh_password: Option<String>,
    #[serde(default)]
    pub keywords: String,
    pub max_viewers: u32,
    #[serde(default)]
    pub current_password: String,
    #[serde(default)]
    pub new_password: String,
    #[serde(default)]
    pub confirm: String,
}

pub async fn save_settings(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<SettingsBody>,
) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    let mut next = state
        .settings
        .read()
        .unwrap()
        .clone()
        .expect("authed implies configured");
    let Some(tvh_url) = config::normalize_tvh_url(&b.tvh_url) else {
        return err(
            StatusCode::BAD_REQUEST,
            "Enter tvheadend's address, like http://192.168.0.149:9981",
        );
    };
    let max_viewers = match config::validate_max_viewers(b.max_viewers) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let password_change = !b.new_password.is_empty();
    if password_change {
        let (current, hash) = (b.current_password.clone(), next.password_hash.clone());
        if !tokio::task::spawn_blocking(move || config::verify_password(&current, &hash))
            .await
            .unwrap_or(false)
        {
            return err(StatusCode::FORBIDDEN, "Your current password isn't right.");
        }
        if let Err(e) = config::validate_credentials(&next.username, &b.new_password, &b.confirm) {
            return err(StatusCode::BAD_REQUEST, e);
        }
        next.password_hash = config::hash_password(&b.new_password);
    }
    let tvh_changed = tvh_url != next.tvh_url
        || b.tvh_username.trim() != next.tvh_username
        || b.tvh_password.is_some();
    next.tvh_url = tvh_url;
    next.tvh_username = b.tvh_username.trim().into();
    if let Some(p) = b.tvh_password {
        next.tvh_password = p;
    }
    next.keywords = config::parse_keywords(&b.keywords);
    next.max_viewers = max_viewers;
    if tvh_changed {
        let probe = tvh::Tvh {
            base: next.tvh_url.clone(),
            username: next.tvh_username.clone(),
            password: next.tvh_password.clone(),
            client: state.http.clone(),
        };
        if let Err(e) = probe.channels().await {
            return err(StatusCode::BAD_GATEWAY, e);
        }
        *state.guide.lock().await = None;
        state.logos.lock().unwrap().clear();
    }
    if let Err(e) = config::save(&state.data_dir, &next) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't save settings: {e}"),
        );
    }
    *state.settings.write().unwrap() = Some(next);
    state.hub.lock().unwrap().set_max(max_viewers);
    if password_change {
        // Sign out every other device; this one stays signed in.
        state
            .sessions
            .lock()
            .unwrap()
            .revoke_all_except(session_token(&headers).as_deref());
    }
    Json(json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------- guide

const GUIDE_TTL: Duration = Duration::from_secs(60);

async fn load_guide(state: &AppState) -> Result<Arc<Guide>, String> {
    let mut cache = state.guide.lock().await;
    if let Some(g) = cache.as_ref()
        && g.fetched.elapsed() < GUIDE_TTL
    {
        return Ok(g.clone());
    }
    let tvh = state.tvh().ok_or("Not set up yet.")?;
    let fresh = async {
        let (channels, events) = tokio::join!(tvh.channels(), tvh.events());
        Ok::<_, String>(Guide {
            fetched: Instant::now(),
            channels: channels?,
            events: events.unwrap_or_default(),
        })
    }
    .await;
    match fresh {
        Ok(g) => {
            let g = Arc::new(g);
            *cache = Some(g.clone());
            Ok(g)
        }
        // tvheadend hiccup: keep serving the last good guide.
        Err(e) => cache.clone().ok_or(e),
    }
}

pub async fn guide(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    let g = match load_guide(&state).await {
        Ok(g) => g,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e),
    };
    let now = now_unix();
    let events: Vec<&tvh::Event> = g
        .events
        .iter()
        .filter(|e| e.stop > now - 1800 && e.start < now + 12 * 3600)
        .collect();
    let keywords = state
        .settings
        .read()
        .unwrap()
        .as_ref()
        .map(|s| s.keywords.clone())
        .unwrap_or_default();
    let pick = tvh::keyword_channel(&keywords, &g.channels, &g.events, now);
    no_store(
        Json(json!({
            "now": now,
            "channels": g.channels,
            "events": events,
            "keyword": pick.map(|(channel, keyword)| json!({ "channel": channel, "keyword": keyword })),
        }))
        .into_response(),
    )
}

pub async fn logo(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(uuid): Path<String>,
) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    if let Some((ctype, bytes)) = state.logos.lock().unwrap().get(&uuid).cloned() {
        return (
            [
                (header::CONTENT_TYPE, ctype),
                (header::CACHE_CONTROL, "private, max-age=86400".into()),
            ],
            bytes,
        )
            .into_response();
    }
    let Ok(g) = load_guide(&state).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(path) = g
        .channels
        .iter()
        .find(|c| c.uuid == uuid)
        .and_then(|c| c.icon_path.clone())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(tvh) = state.tvh() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tvh.icon(&path).await {
        Some((ctype, bytes)) => {
            state
                .logos
                .lock()
                .unwrap()
                .insert(uuid, (ctype.clone(), bytes.clone()));
            (
                [
                    (header::CONTENT_TYPE, ctype),
                    (header::CACHE_CONTROL, "private, max-age=86400".into()),
                ],
                bytes,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------- watching

fn valid_client_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_channel_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn apply(state: &Shared, actions: Vec<Action>) {
    let Some(tvh) = state.tvh() else { return };
    state
        .runner
        .apply(&state.hub, move |ch| tvh.stream_url(ch), actions)
        .await;
}

fn view_json(state: &AppState, client: &str) -> Value {
    let v = state.hub.lock().unwrap().view(client);
    json!(v)
}

#[derive(Deserialize)]
pub struct ClientBody {
    pub client: String,
}

pub async fn heartbeat(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<ClientBody>,
) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    if !valid_client_id(&b.client) {
        return err(StatusCode::BAD_REQUEST, "bad client id");
    }
    state
        .hub
        .lock()
        .unwrap()
        .heartbeat(&b.client, Instant::now());
    no_store(Json(view_json(&state, &b.client)).into_response())
}

#[derive(Deserialize)]
pub struct WatchBody {
    pub client: String,
    pub channel: String,
    #[serde(default)]
    pub retry: bool,
}

pub async fn watch(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<WatchBody>,
) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    if !valid_client_id(&b.client) || !valid_channel_id(&b.channel) {
        return err(StatusCode::BAD_REQUEST, "bad request");
    }
    let result = state
        .hub
        .lock()
        .unwrap()
        .watch(&b.client, &b.channel, b.retry, Instant::now());
    match result {
        Ok(actions) => {
            apply(&state, actions).await;
            no_store(Json(view_json(&state, &b.client)).into_response())
        }
        Err(WatchError::Locked) => {
            let mut body = view_json(&state, &b.client);
            body["error"] =
                json!("Another viewer is in control. Close that one to change the channel.");
            no_store((StatusCode::CONFLICT, Json(body)).into_response())
        }
    }
}

pub async fn leave(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<ClientBody>,
) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    let actions = state.hub.lock().unwrap().leave(&b.client, Instant::now());
    apply(&state, actions).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Serve the HLS playlist / segments. The playlist request waits (up to ~20 s) for
/// ffmpeg's first segment so the player never sees a 404 while the tuner locks on.
pub async fn hls(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path((id, generation, file)): Path<(u64, u64, String)>,
) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    if !stream::is_hls_file(&file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = state.runner.dir(id, generation).join(&file);
    if file == "live.m3u8" {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&path).await
                && text.contains(".ts")
            {
                return (
                    [
                        (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
                        (header::CACHE_CONTROL, "no-cache, no-store"),
                    ],
                    text,
                )
                    .into_response();
            }
            let alive = {
                let hub = state.hub.lock().unwrap();
                hub.is_current(id, generation)
                    && !matches!(hub.view_status_pub(id), Some(stream::Status::Error(_)))
            };
            if !alive || Instant::now() > deadline {
                return StatusCode::NOT_FOUND.into_response();
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "video/mp2t"),
                (header::CACHE_CONTROL, "private, max-age=60"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
