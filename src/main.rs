//! tvh-proxy — a sleek, password-protected live TV front end for tvheadend.
//!
//! tvheadend's own web player can't be exposed to the internet and can't play the
//! raw broadcast on an iPhone. This server sits in front of it: it signs you in,
//! shows a full-screen guide built from tvheadend's EPG, and re-encodes the channel
//! you pick into 1-second HLS segments (via ffmpeg) that Safari plays natively.
//! The frontend is baked into the binary.

mod auth;
mod config;
mod handlers;
mod stream;
#[cfg(test)]
mod tests;
mod tvh;

use axum::{
    Router,
    routing::{get, post},
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Cached channels + EPG, refreshed at most once a minute.
pub struct Guide {
    pub fetched: Instant,
    pub channels: Vec<tvh::Channel>,
    pub events: Vec<tvh::Event>,
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub settings: RwLock<Option<config::Settings>>,
    /// Required to complete first-run setup; printed to the container log.
    pub setup_code: Mutex<Option<String>>,
    pub sessions: Mutex<auth::Sessions>,
    pub limiter: Mutex<auth::LoginLimiter>,
    pub hub: Arc<Mutex<stream::Hub>>,
    pub runner: Arc<stream::Runner>,
    pub guide: tokio::sync::Mutex<Option<Arc<Guide>>>,
    pub logos: Mutex<HashMap<String, (String, Vec<u8>)>>,
    pub http: reqwest::Client,
    /// Force the cookie's `Secure` flag (it's also set automatically behind an HTTPS proxy).
    pub cookie_secure: bool,
    /// Use `X-Forwarded-For` for login throttling (only when behind a reverse proxy you control).
    pub trust_proxy: bool,
}

pub type Shared = Arc<AppState>;

impl AppState {
    pub fn tvh(&self) -> Option<tvh::Tvh> {
        let s = self.settings.read().unwrap();
        s.as_ref().map(|s| tvh::Tvh {
            base: s.tvh_url.clone(),
            username: s.tvh_username.clone(),
            password: s.tvh_password.clone(),
            client: self.http.clone(),
        })
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub fn build_state(data_dir: PathBuf, work_dir: PathBuf, enc: stream::EncoderConfig) -> Shared {
    let settings = config::load(&data_dir);
    let setup_code = if settings.is_none() {
        let code = auth::setup_code();
        println!("\n  ┌──────────────────────────────────────────────┐");
        println!("  │  First-run setup code:  {code}            │");
        println!("  │  Open the app in a browser and enter it.     │");
        println!("  └──────────────────────────────────────────────┘\n");
        Some(code)
    } else {
        None
    };
    let max = settings.as_ref().map(|s| s.max_viewers).unwrap_or(1);
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client");
    Arc::new(AppState {
        sessions: Mutex::new(auth::Sessions::load(Some(data_dir.join("sessions.json")))),
        data_dir,
        settings: RwLock::new(settings),
        setup_code: Mutex::new(setup_code),
        limiter: Mutex::new(auth::LoginLimiter::new()),
        hub: Arc::new(Mutex::new(stream::Hub::new(max))),
        runner: Arc::new(stream::Runner::new(enc, &work_dir).expect("work dir must be writable")),
        guide: tokio::sync::Mutex::new(None),
        logos: Mutex::new(HashMap::new()),
        http,
        cookie_secure: env_flag("COOKIE_SECURE"),
        trust_proxy: env_flag("TRUST_PROXY"),
    })
}

/// Build the app router. Layer-free so tests can drive it via `oneshot`.
pub fn build_router(state: Shared) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route("/manifest.webmanifest", get(handlers::manifest))
        .route("/static/{*filepath}", get(handlers::serve_static))
        .route("/api/status", get(handlers::status))
        .route("/api/setup", post(handlers::setup))
        .route("/api/login", post(handlers::login))
        .route("/api/logout", post(handlers::logout))
        .route(
            "/api/settings",
            get(handlers::get_settings).post(handlers::save_settings),
        )
        .route("/api/guide", get(handlers::guide))
        .route("/api/heartbeat", post(handlers::heartbeat))
        .route("/api/watch", post(handlers::watch))
        .route("/api/leave", post(handlers::leave))
        .route("/api/stop", post(handlers::stop))
        .route("/api/accounts", post(handlers::create_account))
        .route(
            "/api/accounts/{id}",
            post(handlers::update_account).delete(handlers::delete_account),
        )
        .route("/hls/{id}/{generation}/{file}", get(handlers::hls))
        .route("/logo/{uuid}", get(handlers::logo))
        .layer(axum::middleware::map_response(security_headers))
        .with_state(state)
}

/// Baseline hardening for an internet-facing app: no framing (clickjacking), no MIME
/// sniffing, no referrer leaks, and a CSP that only allows our own scripts. Logos may
/// come from tvheadend's absolute icon URLs (https), and hls.js needs blob: workers/media.
async fn security_headers(mut resp: axum::response::Response) -> axum::response::Response {
    use axum::http::{HeaderValue, header};
    let h = resp.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: http:; \
             media-src 'self' blob:; worker-src 'self' blob:; connect-src 'self'; font-src 'self'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    resp
}

fn reset_admin(data_dir: &std::path::Path) -> i32 {
    let Some(mut settings) = config::load(data_dir) else {
        eprintln!(
            "No config.json in {} — open the app to run first-run setup instead.",
            data_dir.display()
        );
        return 1;
    };
    let Some(admin) = settings.accounts.iter_mut().find(|a| a.admin) else {
        eprintln!("No admin account found.");
        return 1;
    };
    let password = auth::random_token()[..16].to_string();
    admin.password_hash = config::hash_password(&password);
    let (id, name) = (admin.id.clone(), admin.username.clone());
    if let Err(e) = config::save(data_dir, &settings) {
        eprintln!("Couldn't save config.json: {e}");
        return 1;
    }
    auth::Sessions::load(Some(data_dir.join("sessions.json"))).revoke_account(&id, None, true);
    println!("Admin \"{name}\" now has the password: {password}");
    println!(
        "Restart the container to apply it (e.g. `docker restart tvh-proxy`), then sign in and change it in Settings."
    );
    0
}

fn port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9982)
}

#[tokio::main]
async fn main() {
    // `healthcheck` subcommand: TCP-connect to our own port, exit 0/1 (no curl in the image).
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let ok = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(("127.0.0.1", port())),
        )
        .await
        .is_ok_and(|r| r.is_ok());
        std::process::exit(if ok { 0 } else { 1 });
    }

    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));

    // `reset-admin`: the way back in if the admin password is lost, without wiping
    // the other accounts. Run inside the container, then restart it.
    if std::env::args().nth(1).as_deref() == Some("reset-admin") {
        std::process::exit(reset_admin(&data_dir));
    }
    let work_dir = std::env::temp_dir().join("tvh-proxy");
    let state = build_state(data_dir, work_dir, stream::EncoderConfig::from_env());

    // Housekeeping: drop vanished tabs, promote followers, stop idle streams.
    let st = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tick.tick().await;
            let actions = st.hub.lock().unwrap().tick(Instant::now());
            if !actions.is_empty() {
                handlers::apply(&st, actions).await;
            }
        }
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], port()));
    println!("tvh-proxy listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    let shutdown_state = state.clone();
    axum::serve(
        listener,
        build_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal");
            tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
        }
        #[cfg(not(unix))]
        let _ = ctrl_c.await;
        shutdown_state.runner.shutdown().await;
    })
    .await
    .expect("server");
}
