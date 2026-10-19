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

/// The account behind this request's session cookie, if any. Pre-accounts sessions
/// belong to the admin.
fn current_account(state: &AppState, headers: &HeaderMap) -> Option<config::Account> {
    let token = session_token(headers)?;
    let id = state.sessions.lock().unwrap().account_for(&token)?;
    let settings = state.settings.read().unwrap();
    let s = settings.as_ref()?;
    if id.is_empty() {
        s.first_admin().cloned()
    } else {
        s.account(&id).cloned()
    }
}

/// Gate for protected routes; yields the signed-in account.
#[allow(clippy::result_large_err)]
fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<config::Account, Response> {
    current_account(state, headers).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Please sign in."))
}

/// Gate for settings and account management.
#[allow(clippy::result_large_err)]
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<config::Account, Response> {
    let me = require_auth(state, headers)?;
    if me.admin {
        Ok(me)
    } else {
        Err(err(
            StatusCode::FORBIDDEN,
            "Only the admin can change settings.",
        ))
    }
}

/// A fixed argon2 hash to verify against when the username doesn't exist, so an
/// unknown username takes as long as a wrong password.
fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| config::hash_password("not-a-real-password"))
}

#[allow(clippy::result_large_err)]
fn save_settings_to_disk(state: &AppState, next: config::Settings) -> Result<(), Response> {
    if let Err(e) = config::save(&state.data_dir, &next) {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't save settings: {e}"),
        ));
    }
    *state.settings.write().unwrap() = Some(next);
    Ok(())
}

/// Who to throttle: the connecting address. Forwarded-for headers are deliberately
/// ignored, since they can be forged; behind a tunnel every remote visitor shares the
/// tunnel's bucket, which only makes guessing slower.
fn client_key(parts: &Parts) -> String {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// A failed sign-in (or setup code): one log line, plus a POST to the admin's webhook
/// if one is set. Never delays the response. The login throttle caps how often this
/// can happen, so neither the log nor the webhook can be flooded.
fn report_failed_signin(state: &Shared, parts: &Parts, key: &str, username: &str) {
    let ip = client_key(parts);
    // Informational only (it can be forged): who the tunnel or proxy says connected.
    let forwarded = ["cf-connecting-ip", "x-forwarded-for"]
        .iter()
        .find_map(|h| parts.headers.get(*h)?.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().chars().take(64).collect::<String>())
        .filter(|v| !v.is_empty());
    let username: String = username.trim().chars().take(64).collect();
    let locked_out = state
        .limiter
        .lock()
        .unwrap()
        .check(key, Instant::now())
        .err()
        .unwrap_or(0);
    // {:?} escapes quotes and control characters, so a username can't forge log lines.
    eprintln!(
        "failed sign-in for {username:?} from {ip}{}; locked out {locked_out}s",
        forwarded
            .as_ref()
            .map(|f| format!(" (forwarded-for {f:?})"))
            .unwrap_or_default()
    );
    let url = state
        .settings
        .read()
        .unwrap()
        .as_ref()
        .map(|s| s.alert_webhook.clone())
        .unwrap_or_default();
    if url.is_empty() {
        return;
    }
    let body = json!({
        "event": "login_failed",
        "username": username,
        "ip": ip,
        "forwarded_for": forwarded,
        "locked_out_seconds": locked_out,
        "time": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    });
    let http = state.http.clone();
    tokio::spawn(async move {
        let sent = http
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .and_then(|r| r.error_for_status());
        if let Err(e) = sent {
            eprintln!(
                "failed sign-in webhook didn't go through: {}",
                e.without_url()
            );
        }
    });
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
    let me = current_account(&state, &headers);
    let r = no_store(
        Json(json!({
            "configured": configured,
            "authed": me.is_some(),
            "username": me.as_ref().map(|a| a.username.clone()),
            "admin": me.as_ref().is_some_and(|a| a.admin),
            "active_clients": state.hub.lock().unwrap().active_clients(),
        }))
        .into_response(),
    );
    // The app checks status on launch and when it returns to the foreground, so this
    // keeps a device that's in use signed in indefinitely.
    match session_token(&headers) {
        Some(token) if me.is_some() && state.sessions.lock().unwrap().renew(&token) => {
            with_cookie(r, auth::session_cookie(&token, is_https(&state, &headers)))
        }
        _ => r,
    }
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
}

pub async fn setup(
    State(state): State<Shared>,
    parts: Parts,
    Json(b): Json<SetupBody>,
) -> Response {
    if state.settings.read().unwrap().is_some() {
        return err(StatusCode::CONFLICT, "Already set up. Sign in instead.");
    }
    let key = format!("setup:{}", client_key(&parts));
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
        report_failed_signin(&state, &parts, &key, "(setup code)");
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
    let probe = tvh::Tvh {
        base: tvh_url.clone(),
        username: b.tvh_username.trim().into(),
        password: b.tvh_password.clone(),
        client: state.http.clone(),
    };
    if let Err(e) = probe.channels().await {
        return err(StatusCode::BAD_GATEWAY, e);
    }
    let admin = config::Account {
        id: config::new_account_id(),
        username: b.username.trim().into(),
        password_hash: config::hash_password(&b.password),
        admin: true,
        priority: false,
    };
    let admin_id = admin.id.clone();
    let settings = config::Settings {
        tvh_url,
        tvh_username: b.tvh_username.trim().into(),
        tvh_password: b.tvh_password,
        accounts: vec![admin],
        username: None,
        password_hash: None,
        keywords: config::parse_keywords(&b.keywords),
        alert_webhook: String::new(),
    };
    if let Err(e) = config::save(&state.data_dir, &settings) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't save settings: {e}"),
        );
    }
    *state.settings.write().unwrap() = Some(settings);
    *state.setup_code.lock().unwrap() = None;
    let token = state.sessions.lock().unwrap().create(&admin_id);
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
    let Some(account) = state
        .settings
        .read()
        .unwrap()
        .as_ref()
        .map(|s| s.account_by_name(&b.username).cloned())
    else {
        return err(StatusCode::CONFLICT, "Finish setup first.");
    };
    let key = client_key(&parts);
    if let Err(secs) = state.limiter.lock().unwrap().check(&key, Instant::now()) {
        return throttled(secs);
    }
    // Always run the (slow) hash check so an unknown username takes as long as a wrong password.
    let password = b.password;
    let hash = account
        .as_ref()
        .map(|a| a.password_hash.clone())
        .unwrap_or_else(|| dummy_hash().to_string());
    let hash_ok = tokio::task::spawn_blocking(move || config::verify_password(&password, &hash))
        .await
        .unwrap_or(false);
    let Some(account) = account.filter(|_| hash_ok) else {
        state
            .limiter
            .lock()
            .unwrap()
            .record_failure(&key, Instant::now());
        report_failed_signin(&state, &parts, &key, &b.username);
        tokio::time::sleep(Duration::from_millis(400)).await;
        return err(StatusCode::UNAUTHORIZED, "Wrong username or password.");
    };
    state.limiter.lock().unwrap().record_success(&key);
    let token = state.sessions.lock().unwrap().create(&account.id);
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
    let me = match require_admin(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
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
            "username": me.username,
            "keywords": s.keywords.join(", "),
            "alert_webhook": s.alert_webhook,
            "accounts": accounts_json(&s, &me),
        }))
        .into_response(),
    )
}

fn accounts_json(s: &config::Settings, me: &config::Account) -> Value {
    json!(s
        .accounts
        .iter()
        .map(|a| json!({ "id": a.id, "username": a.username, "admin": a.admin, "priority": a.priority, "me": a.id == me.id }))
        .collect::<Vec<_>>())
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
    /// `None` keeps the stored webhook; `""` turns it off.
    pub alert_webhook: Option<String>,
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
    let me = match require_admin(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
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
    let password_change = !b.new_password.is_empty();
    if password_change {
        let (current, hash) = (b.current_password.clone(), me.password_hash.clone());
        if !tokio::task::spawn_blocking(move || config::verify_password(&current, &hash))
            .await
            .unwrap_or(false)
        {
            return err(StatusCode::FORBIDDEN, "Your current password isn't right.");
        }
        if let Err(e) = config::validate_credentials(&me.username, &b.new_password, &b.confirm) {
            return err(StatusCode::BAD_REQUEST, e);
        }
        let hash = config::hash_password(&b.new_password);
        if let Some(a) = next.accounts.iter_mut().find(|a| a.id == me.id) {
            a.password_hash = hash;
        }
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
    if let Some(raw) = &b.alert_webhook {
        let Some(url) = config::normalize_webhook(raw) else {
            return err(
                StatusCode::BAD_REQUEST,
                "The webhook needs to be a full URL, like http://homeassistant.local:8123/api/webhook/…",
            );
        };
        next.alert_webhook = url;
    }
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
    if let Err(r) = save_settings_to_disk(&state, next) {
        return r;
    }
    if password_change {
        // Sign this account out everywhere else; this device stays signed in.
        state.sessions.lock().unwrap().revoke_account(
            &me.id,
            session_token(&headers).as_deref(),
            true,
        );
    }
    Json(json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------- accounts (admin)

#[derive(Deserialize)]
pub struct NewAccountBody {
    pub username: String,
    pub password: String,
    pub confirm: String,
    #[serde(default)]
    pub priority: bool,
}

pub async fn create_account(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<NewAccountBody>,
) -> Response {
    let me = match require_admin(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let mut next = state.settings.read().unwrap().clone().expect("configured");
    let username = match config::validate_new_username(&next, &b.username) {
        Ok(u) => u,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if let Err(e) = config::validate_credentials(&username, &b.password, &b.confirm) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    let password = b.password.clone();
    let hash = tokio::task::spawn_blocking(move || config::hash_password(&password))
        .await
        .expect("hash");
    let id = config::new_account_id();
    next.accounts.push(config::Account {
        id: id.clone(),
        username,
        password_hash: hash,
        admin: false,
        priority: false,
    });
    if b.priority {
        next.set_priority(Some(&id));
    }
    let body = accounts_json(&next, &me);
    if let Err(r) = save_settings_to_disk(&state, next) {
        return r;
    }
    no_store(Json(json!({ "accounts": body })).into_response())
}

#[derive(Deserialize)]
pub struct UpdateAccountBody {
    /// Make this the priority account (`true`) or clear it (`false`); absent = unchanged.
    pub priority: Option<bool>,
    /// Set a new password (admin reset); absent = unchanged.
    pub password: Option<String>,
    #[serde(default)]
    pub confirm: String,
}

pub async fn update_account(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<UpdateAccountBody>,
) -> Response {
    let me = match require_admin(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let mut next = state.settings.read().unwrap().clone().expect("configured");
    let Some(target) = next.account(&id).cloned() else {
        return err(StatusCode::NOT_FOUND, "No such account.");
    };
    let mut new_hash = None;
    if let Some(pw) = &b.password {
        if let Err(e) = config::validate_credentials(&target.username, pw, &b.confirm) {
            return err(StatusCode::BAD_REQUEST, e);
        }
        let pw = pw.clone();
        new_hash = Some(
            tokio::task::spawn_blocking(move || config::hash_password(&pw))
                .await
                .expect("hash"),
        );
    }
    if let Some(h) = new_hash.clone()
        && let Some(a) = next.accounts.iter_mut().find(|a| a.id == id)
    {
        a.password_hash = h;
    }
    match b.priority {
        Some(true) => next.set_priority(Some(&id)),
        Some(false) if target.priority => next.set_priority(None),
        _ => {}
    }
    let body = accounts_json(&next, &me);
    if let Err(r) = save_settings_to_disk(&state, next) {
        return r;
    }
    if new_hash.is_some() {
        // A reset signs that account out everywhere (except the admin's own device
        // when resetting their own password here).
        let keep = if id == me.id {
            session_token(&headers)
        } else {
            None
        };
        state
            .sessions
            .lock()
            .unwrap()
            .revoke_account(&id, keep.as_deref(), target.admin);
    }
    no_store(Json(json!({ "accounts": body })).into_response())
}

pub async fn delete_account(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let me = match require_admin(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let mut next = state.settings.read().unwrap().clone().expect("configured");
    let Some(target) = next.account(&id).cloned() else {
        return err(StatusCode::NOT_FOUND, "No such account.");
    };
    if target.admin {
        return err(
            StatusCode::BAD_REQUEST,
            "The admin account can't be deleted.",
        );
    }
    next.accounts.retain(|a| a.id != id);
    let body = accounts_json(&next, &me);
    if let Err(r) = save_settings_to_disk(&state, next) {
        return r;
    }
    state
        .sessions
        .lock()
        .unwrap()
        .revoke_account(&id, None, false);
    no_store(Json(json!({ "accounts": body })).into_response())
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
    let me = match require_auth(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    if !valid_client_id(&b.client) {
        return err(StatusCode::BAD_REQUEST, "bad client id");
    }
    {
        let mut hub = state.hub.lock().unwrap();
        hub.mark_priority(&b.client, me.priority);
        hub.heartbeat(&b.client, Instant::now());
    }
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
    let me = match require_auth(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    if !valid_client_id(&b.client) || !valid_channel_id(&b.channel) {
        return err(StatusCode::BAD_REQUEST, "bad request");
    }
    let result = {
        let mut hub = state.hub.lock().unwrap();
        hub.mark_priority(&b.client, me.priority);
        hub.watch(&b.client, &b.channel, b.retry, Instant::now())
    };
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

/// The player's Stop button.
pub async fn stop(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<ClientBody>,
) -> Response {
    let me = match require_auth(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    if !valid_client_id(&b.client) {
        return err(StatusCode::BAD_REQUEST, "bad client id");
    }
    let actions = {
        let mut hub = state.hub.lock().unwrap();
        hub.mark_priority(&b.client, me.priority);
        hub.stop(&b.client, Instant::now())
    };
    apply(&state, actions).await;
    no_store(Json(view_json(&state, &b.client)).into_response())
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
    let dir = state.runner.dir(id, generation);
    let path = dir.join(&file);
    if file.ends_with(".m3u8") {
        // Hold the first request until every rendition has a segment, so players
        // never see an empty playlist.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if stream::playlists_ready(&dir).await
                && let Ok(text) = tokio::fs::read_to_string(&path).await
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
