use crate::stream::{Action, CLIENT_TIMEOUT, Hub, IDLE_GRACE, Role, Status, WatchError};
use crate::{auth, build_router, build_state, config, stream, tvh};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tower::ServiceExt;

// ---------------------------------------------------------------- config

#[test]
fn normalizes_tvh_urls() {
    assert_eq!(
        config::normalize_tvh_url("192.168.0.149:9981").as_deref(),
        Some("http://192.168.0.149:9981")
    );
    assert_eq!(
        config::normalize_tvh_url(" http://tv.lan:9981/ ").as_deref(),
        Some("http://tv.lan:9981")
    );
    assert_eq!(
        config::normalize_tvh_url("https://tv.example.com").as_deref(),
        Some("https://tv.example.com")
    );
    assert_eq!(config::normalize_tvh_url(""), None);
    assert_eq!(config::normalize_tvh_url("http://"), None);
    assert_eq!(config::normalize_tvh_url("not a url"), None);
}

#[test]
fn parses_keywords_in_order_lowercased_and_deduped() {
    assert_eq!(
        config::parse_keywords("NFL, olympics ,, nfl, World Cup "),
        vec!["nfl", "olympics", "world cup"]
    );
    assert!(config::parse_keywords(" , ").is_empty());
}

#[test]
fn validates_credentials_and_viewers() {
    assert!(config::validate_credentials("me", "longenough", "longenough").is_ok());
    assert!(config::validate_credentials("", "longenough", "longenough").is_err());
    assert!(config::validate_credentials("me", "short", "short").is_err());
    assert!(config::validate_credentials("me", "longenough", "different1").is_err());
    assert!(config::validate_max_viewers(0).is_err());
    assert_eq!(config::validate_max_viewers(1), Ok(1));
    assert!(config::validate_max_viewers(config::MAX_VIEWERS_LIMIT + 1).is_err());
}

#[test]
fn password_hash_round_trips() {
    let h = config::hash_password("correct horse");
    assert!(config::verify_password("correct horse", &h));
    assert!(!config::verify_password("wrong horse", &h));
    assert!(!config::verify_password("x", "not-a-hash"));
}

// ---------------------------------------------------------------- tvheadend parsing & keywords

fn chans() -> Vec<tvh::Channel> {
    tvh::parse_channels(&json!({ "entries": [
        { "uuid": "bb", "name": "KPIX-TV", "number": "5.1", "icon_public_url": "imagecache/7", "enabled": true },
        { "uuid": "aa", "name": "KTVU-HD", "number": "2.1", "icon_public_url": "https://x/ktvu.png" },
        { "uuid": "cc", "name": "KNTV-HD", "number": "11.3" },
        { "uuid": "dd", "name": "Off", "number": "1.1", "enabled": false },
    ]}))
}

#[test]
fn channels_sort_numerically_and_skip_disabled() {
    let c = chans();
    let names: Vec<&str> = c.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["KTVU-HD", "KPIX-TV", "KNTV-HD"]);
    assert_eq!(c[0].icon.as_deref(), Some("https://x/ktvu.png"));
    assert_eq!(c[1].icon.as_deref(), Some("/logo/bb"));
    assert_eq!(c[1].icon_path.as_deref(), Some("imagecache/7"));
    assert_eq!(c[2].icon, None);
    assert!(tvh::channel_sort_key("10.1") > tvh::channel_sort_key("9.9"));
    assert!(tvh::channel_sort_key("") > tvh::channel_sort_key("99"));
}

fn events() -> Vec<tvh::Event> {
    tvh::parse_events(&json!({ "entries": [
        { "channelUuid": "cc", "start": 100, "stop": 200, "title": "Sunday Night Football", "description": "Cowboys at Packers", "genre": [64] },
        { "channelUuid": "aa", "start": 100, "stop": 200, "title": "News", "description": "Includes NFL highlights" },
        { "channelUuid": "bb", "start": 200, "stop": 300, "title": "Olympics Tonight" },
        { "channelUuid": "bb", "start": 50, "stop": 50, "title": "zero length (dropped)" },
    ]}))
}

#[test]
fn parses_events_with_categories() {
    let e = events();
    assert_eq!(e.len(), 3);
    assert_eq!(e[0].category, Some("Sports"));
    assert_eq!(e[1].category, None);
}

#[test]
fn keyword_picks_first_keyword_then_first_channel_on_air_now() {
    let (c, e) = (chans(), events());
    let kw = |s: &str| config::parse_keywords(s);
    // "nfl" matches KTVU's description (2.1 comes before 11.3).
    assert_eq!(
        tvh::keyword_channel(&kw("nfl"), &c, &e, 150),
        Some(("aa".into(), "nfl".into()))
    );
    // Category matches too, and keyword order wins over channel order.
    assert_eq!(
        tvh::keyword_channel(&kw("sports, nfl"), &c, &e, 150),
        Some(("cc".into(), "sports".into()))
    );
    // Case-insensitive.
    assert_eq!(
        tvh::keyword_channel(&kw("PACKERS"), &c, &e, 150),
        Some(("cc".into(), "packers".into()))
    );
    // Only what's airing now counts: Olympics starts at 200.
    assert_eq!(tvh::keyword_channel(&kw("olympics"), &c, &e, 150), None);
    assert_eq!(
        tvh::keyword_channel(&kw("olympics, nfl"), &c, &e, 250),
        Some(("bb".into(), "olympics".into()))
    );
    assert_eq!(tvh::keyword_channel(&[], &c, &e, 150), None);
}

#[test]
fn stream_url_carries_encoded_credentials() {
    let mut t = tvh::Tvh {
        base: "http://tv.lan:9981".into(),
        username: String::new(),
        password: String::new(),
        client: reqwest::Client::new(),
    };
    assert_eq!(
        t.stream_url("abc"),
        "http://tv.lan:9981/stream/channel/abc?profile=pass"
    );
    t.username = "me".into();
    t.password = "p@ss word".into();
    assert_eq!(
        t.stream_url("abc"),
        "http://me:p%40ss%20word@tv.lan:9981/stream/channel/abc?profile=pass"
    );
}

// ---------------------------------------------------------------- hub: who controls what

fn started(actions: &[Action]) -> Vec<(u64, u64, String)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Start {
                id,
                generation,
                channel,
            } => Some((*id, *generation, channel.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn first_viewer_controls_and_second_follows_with_one_slot() {
    let t0 = Instant::now();
    let mut hub = Hub::new(1);
    let a = hub.watch("tab-a", "aa", false, t0).unwrap();
    assert_eq!(started(&a), vec![(1, 1, "aa".into())]);
    assert_eq!(hub.view("tab-a").role, Role::Controller);

    // A second device signs in: it is attached to the running stream immediately.
    hub.heartbeat("tab-b", t0);
    let vb = hub.view("tab-b");
    assert_eq!(vb.role, Role::Follower);
    assert_eq!(vb.stream.as_ref().unwrap().channel, "aa");
    assert_eq!(hub.view("tab-a").others, 1);

    // ...and can't change the channel.
    assert_eq!(hub.watch("tab-b", "bb", false, t0), Err(WatchError::Locked));

    // The controller changes channel; the follower's view follows (new generation).
    let a2 = hub.watch("tab-a", "bb", false, t0).unwrap();
    assert_eq!(started(&a2), vec![(1, 2, "bb".into())]);
    let vb = hub.view("tab-b");
    assert_eq!(vb.stream.as_ref().unwrap().channel, "bb");
    assert_eq!(vb.stream.as_ref().unwrap().playlist, "/hls/1/2/live.m3u8");
}

#[test]
fn rewatching_the_same_channel_is_a_no_op_unless_retrying_after_error() {
    let t0 = Instant::now();
    let mut hub = Hub::new(1);
    hub.watch("tab-a", "aa", false, t0).unwrap();
    assert!(hub.watch("tab-a", "aa", false, t0).unwrap().is_empty());
    assert!(
        hub.watch("tab-a", "aa", true, t0).unwrap().is_empty(),
        "retry only restarts a failed stream"
    );
    hub.set_status(1, 1, Status::Error("tuner".into()));
    assert_eq!(
        started(&hub.watch("tab-a", "aa", true, t0).unwrap()),
        vec![(1, 2, "aa".into())]
    );
    assert_eq!(hub.view("tab-a").stream.unwrap().status, Status::Starting);
}

#[test]
fn control_passes_to_the_longest_waiting_follower() {
    let t0 = Instant::now();
    let mut hub = Hub::new(1);
    hub.watch("tab-a", "aa", false, t0).unwrap();
    hub.heartbeat("tab-b", t0 + Duration::from_secs(1));
    hub.heartbeat("tab-c", t0 + Duration::from_secs(2));
    // tab-a closes its page.
    assert!(
        hub.leave("tab-a", t0 + Duration::from_secs(3)).is_empty(),
        "stream keeps running"
    );
    assert_eq!(hub.view("tab-b").role, Role::Controller);
    assert_eq!(hub.view("tab-c").role, Role::Follower);
    assert!(
        !hub.watch("tab-b", "bb", false, t0 + Duration::from_secs(4))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn silent_tabs_time_out_and_idle_streams_stop_after_grace() {
    let t0 = Instant::now();
    let mut hub = Hub::new(1);
    hub.watch("tab-a", "aa", false, t0).unwrap();
    // No heartbeats: the tab is dropped, then the stream lingers for the grace period.
    let t1 = t0 + CLIENT_TIMEOUT + Duration::from_secs(1);
    assert!(hub.tick(t1).is_empty());
    assert_eq!(hub.view("tab-a").role, Role::Idle);
    // A reload within the grace period re-attaches without retuning.
    hub.heartbeat("tab-a2", t1 + Duration::from_secs(5));
    assert_eq!(hub.view("tab-a2").role, Role::Follower);
    // ...and with nobody else there, it is promoted on the next tick.
    hub.tick(t1 + Duration::from_secs(6));
    assert_eq!(hub.view("tab-a2").role, Role::Controller);
    // Everyone leaves; after the grace period the stream is stopped.
    hub.leave("tab-a2", t1 + Duration::from_secs(7));
    assert!(hub.tick(t1 + Duration::from_secs(8)).is_empty());
    assert_eq!(
        hub.tick(t1 + Duration::from_secs(8) + IDLE_GRACE),
        vec![Action::Stop { id: 1 }]
    );
    // The slot is free again.
    hub.heartbeat("tab-z", t1 + Duration::from_secs(60));
    assert_eq!(hub.view("tab-z").role, Role::Idle);
}

#[test]
fn more_slots_allow_independent_channels() {
    let t0 = Instant::now();
    let mut hub = Hub::new(2);
    hub.watch("tab-a", "aa", false, t0).unwrap();
    hub.heartbeat("tab-b", t0);
    assert_eq!(
        hub.view("tab-b").role,
        Role::Idle,
        "a free slot means you pick your own channel"
    );
    assert_eq!(
        started(&hub.watch("tab-b", "bb", false, t0).unwrap()),
        vec![(2, 1, "bb".into())]
    );
    // Now full: a third tab follows the oldest stream.
    hub.heartbeat("tab-c", t0);
    assert_eq!(hub.view("tab-c").stream.unwrap().channel, "aa");
    assert_eq!(hub.watch("tab-c", "cc", false, t0), Err(WatchError::Locked));
}

#[test]
fn status_updates_ignore_stale_generations() {
    let t0 = Instant::now();
    let mut hub = Hub::new(1);
    hub.watch("tab-a", "aa", false, t0).unwrap();
    hub.watch("tab-a", "bb", false, t0).unwrap(); // generation 2
    hub.set_status(1, 1, Status::Error("old ffmpeg died".into()));
    assert_eq!(hub.view("tab-a").stream.unwrap().status, Status::Starting);
    hub.set_status(1, 2, Status::Live);
    assert_eq!(hub.view("tab-a").stream.unwrap().status, Status::Live);
}

// ---------------------------------------------------------------- ffmpeg & hls files

fn enc(vaapi: bool) -> stream::EncoderConfig {
    stream::EncoderConfig {
        ffmpeg: "ffmpeg".into(),
        hwaccel_vaapi: vaapi,
        vaapi_device: "/dev/dri/renderD128".into(),
        height: 720,
        video_bitrate_k: 3500,
    }
}

#[test]
fn ffmpeg_args_make_one_second_hls_segments() {
    let args =
        stream::ffmpeg_args(&enc(false), "http://tv/stream", &PathBuf::from("/w/1/1")).join(" ");
    assert!(args.contains("-i http://tv/stream"));
    assert!(args.contains("-c:v libx264"));
    assert!(args.contains("-hls_time 1"));
    assert!(args.contains("-g 30"));
    assert!(args.contains("-c:a aac -ac 2"));
    assert!(args.ends_with("/w/1/1/seg%05d.ts /w/1/1/live.m3u8"));
    let hw = stream::ffmpeg_args(&enc(true), "http://tv/stream", &PathBuf::from("/w")).join(" ");
    assert!(hw.contains("-vaapi_device /dev/dri/renderD128"));
    assert!(hw.contains("h264_vaapi"));
    assert!(hw.contains("hwupload"));
}

#[test]
fn only_hls_file_names_are_served() {
    assert!(stream::is_hls_file("live.m3u8"));
    assert!(stream::is_hls_file("seg00042.ts"));
    assert!(!stream::is_hls_file("seg.ts"));
    assert!(!stream::is_hls_file("../config.json"));
    assert!(!stream::is_hls_file("seg1.ts/../../x"));
    assert!(!stream::is_hls_file("segab.ts"));
}

// ---------------------------------------------------------------- auth helpers

#[test]
fn login_limiter_locks_out_after_free_attempts() {
    let t0 = Instant::now();
    let mut l = auth::LoginLimiter::new();
    for _ in 0..auth::FREE_ATTEMPTS - 1 {
        assert!(l.check("1.2.3.4", t0).is_ok());
        l.record_failure("1.2.3.4", t0);
    }
    assert!(l.check("1.2.3.4", t0).is_ok());
    l.record_failure("1.2.3.4", t0);
    let wait = l.check("1.2.3.4", t0).unwrap_err();
    assert!((59..=60).contains(&wait));
    assert!(l.check("5.6.7.8", t0).is_ok(), "other clients unaffected");
    assert!(l.check("1.2.3.4", t0 + Duration::from_secs(61)).is_ok());
    l.record_success("1.2.3.4");
    assert!(l.check("1.2.3.4", t0).is_ok());
}

#[test]
fn cookie_parsing_and_session_lifecycle() {
    assert_eq!(
        auth::token_from_cookie_header("a=1; tvhp_session=abc; b=2"),
        Some("abc")
    );
    assert_eq!(auth::token_from_cookie_header("tvhp_session="), None);
    let mut s = auth::Sessions::load(None);
    let t = s.create();
    assert!(s.is_valid(&t));
    assert!(!s.is_valid("nope"));
    s.revoke(&t);
    assert!(!s.is_valid(&t));
    assert!(auth::session_cookie("x", true).contains("; Secure"));
    assert!(auth::session_cookie("x", false).contains("HttpOnly; SameSite=Lax"));
    let code = auth::setup_code();
    assert_eq!(code.len(), 9);
    assert!(!code.contains('0') && !code.contains('O'));
}

// ---------------------------------------------------------------- router end-to-end

fn temp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "tvhp-test-{tag}-{}",
        auth::random_token().get(..10).unwrap()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// A tiny stand-in for tvheadend's API.
async fn fake_tvh() -> String {
    use axum::{Json, Router, routing::get};
    let app = Router::new()
        .route("/api/channel/grid", get(|| async { Json(json!({ "entries": [{ "uuid": "aa", "name": "KTVU-HD", "number": "2.1" }] })) }))
        .route(
            "/api/epg/events/grid",
            get(|| async {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
                Json(json!({ "entries": [{ "channelUuid": "aa", "start": now - 60, "stop": now + 3600, "title": "Sunday Night Football" }] }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    cookie: Option<&str>,
) -> (StatusCode, Value, Option<String>) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    let req = match body {
        Some(b) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        set_cookie,
    )
}

fn setup_body(code: &str, tvh_url: &str) -> Value {
    json!({
        "setup_code": code, "tvh_url": tvh_url, "username": "akshay", "password": "hunter2hunter2",
        "confirm": "hunter2hunter2", "keywords": "NFL, football", "max_viewers": 1
    })
}

#[tokio::test]
async fn full_setup_login_and_guide_flow() {
    let data = temp_dir("data");
    let state = build_state(data.clone(), temp_dir("work"), enc(false));
    let code = state.setup_code.lock().unwrap().clone().unwrap();
    let app = build_router(state.clone());
    let tvh_url = fake_tvh().await;

    let (s, v, _) = send(&app, "GET", "/api/status", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "configured": false, "authed": false }));

    // Protected routes are closed before setup.
    let (s, _, _) = send(&app, "GET", "/api/guide", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Wrong setup code.
    let (s, _, _) = send(
        &app,
        "POST",
        "/api/setup",
        Some(setup_body("AAAA-BBBB", &tvh_url)),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Mismatched passwords.
    let mut bad = setup_body(&code, &tvh_url);
    bad["confirm"] = json!("nope-nope-nope");
    let (s, v, _) = send(&app, "POST", "/api/setup", Some(bad), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("match"));

    // Success signs you in.
    let (s, _, cookie) = send(
        &app,
        "POST",
        "/api/setup",
        Some(setup_body(&code.to_lowercase(), &tvh_url)),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let cookie = cookie.expect("session cookie");
    let saved = config::load(&data).unwrap();
    assert_eq!(saved.keywords, vec!["nfl", "football"]);
    assert!(
        !std::fs::read_to_string(data.join("config.json"))
            .unwrap()
            .contains("hunter2"),
        "password is hashed"
    );

    // Setup can't be redone.
    let (s, _, _) = send(
        &app,
        "POST",
        "/api/setup",
        Some(setup_body(&code, &tvh_url)),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Guide works and the keyword picks the football channel.
    let (s, v, _) = send(&app, "GET", "/api/guide", None, Some(&cookie)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["channels"][0]["name"], "KTVU-HD");
    assert_eq!(
        v["keyword"],
        json!({ "channel": "aa", "keyword": "football" })
    );

    // Settings never leak the password hash.
    let (s, v, _) = send(&app, "GET", "/api/settings", None, Some(&cookie)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("password_hash").is_none());
    assert_eq!(v["keywords"], "nfl, football");

    // Logout, then a wrong and a right login.
    let (s, _, _) = send(&app, "POST", "/api/logout", Some(json!({})), Some(&cookie)).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = send(&app, "GET", "/api/guide", None, Some(&cookie)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = send(
        &app,
        "POST",
        "/api/login",
        Some(json!({ "username": "akshay", "password": "wrong-password" })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, cookie2) = send(
        &app,
        "POST",
        "/api/login",
        Some(json!({ "username": "akshay", "password": "hunter2hunter2" })),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let cookie2 = cookie2.unwrap();

    // Heartbeat with nothing playing.
    let (s, v, _) = send(
        &app,
        "POST",
        "/api/heartbeat",
        Some(json!({ "client": "tab-aaaaaaaa" })),
        Some(&cookie2),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["role"], "idle");

    // Sessions survive a restart (new state over the same data dir).
    let app2 = build_router(build_state(data.clone(), temp_dir("work2"), enc(false)));
    let (_, v, _) = send(&app2, "GET", "/api/status", None, Some(&cookie2)).await;
    assert_eq!(v, json!({ "configured": true, "authed": true }));
}

#[tokio::test]
async fn hls_route_requires_auth_and_rejects_odd_names() {
    let state = build_state(temp_dir("data"), temp_dir("work"), enc(false));
    let app = build_router(state.clone());
    let (s, _, _) = send(&app, "GET", "/hls/1/1/live.m3u8", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = send(&app, "GET", "/static/js/app.js", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = send(&app, "GET", "/static/../Cargo.toml", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pages_carry_security_headers() {
    let app = build_router(build_state(temp_dir("data"), temp_dir("work"), enc(false)));
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let h = resp.headers();
    assert_eq!(h["x-frame-options"], "DENY");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert!(
        h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    assert_eq!(h["cache-control"], "no-store");
}

#[tokio::test]
async fn json_only_posts() {
    let state = build_state(temp_dir("data"), temp_dir("work"), enc(false));
    let app = build_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("username=a&password=b"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[test]
fn digest_auth_matches_rfc_2617_example() {
    let c = tvh::parse_digest_challenge(
        r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
    )
    .unwrap();
    assert_eq!(c["realm"], "testrealm@host.com");
    let h = tvh::digest_authorization(
        "Mufasa",
        "Circle Of Life",
        "GET",
        "/dir/index.html",
        &c,
        "0a4f113b",
    );
    assert!(
        h.contains(r#"response="6629fae49393a05397450978507c4ef1""#),
        "{h}"
    );
    assert!(h.contains("qop=auth, nc=00000001"));
    assert!(h.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#));
    // tvheadend's unquoted-qop style also parses.
    let c2 =
        tvh::parse_digest_challenge(r#"Digest realm="tvheadend", qop=auth, nonce="abc=""#).unwrap();
    assert_eq!(c2["qop"], "auth");
    assert_eq!(c2["nonce"], "abc=");
    assert!(tvh::parse_digest_challenge("Basic realm=x").is_none());
}

/// Opt-in check against a real tvheadend:
/// `TVH_TEST_URL=http://host:9981 TVH_TEST_USER=.. TVH_TEST_PASS=.. cargo test live_tvheadend -- --ignored`
#[tokio::test]
#[ignore]
async fn live_tvheadend_login() {
    let t = tvh::Tvh {
        base: std::env::var("TVH_TEST_URL").expect("TVH_TEST_URL"),
        username: std::env::var("TVH_TEST_USER").unwrap_or_default(),
        password: std::env::var("TVH_TEST_PASS").unwrap_or_default(),
        client: reqwest::Client::new(),
    };
    let channels = t.channels().await.expect("tvheadend accepted our login");
    assert!(!channels.is_empty());
    // An admin-only endpoint forces the Digest handshake even where anonymous LAN
    // access is allowed for the channel list.
    if !t.username.is_empty() {
        t.json("api/config/load")
            .await
            .expect("Digest login to an admin endpoint");
    }
    let mut wrong = t.clone();
    wrong.password = "definitely-not-the-password".into();
    if !wrong.username.is_empty() {
        assert!(wrong.channels().await.is_err() || std::env::var("TVH_TEST_ANON_OK").is_ok());
    }
}
