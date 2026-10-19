//! Login sessions and brute-force throttling.
//!
//! Sessions are random 256-bit tokens handed to the browser in an HttpOnly cookie.
//! Only their SHA-256 is kept (in memory and in `$DATA_DIR/sessions.json`), so a
//! leaked sessions file can't be replayed as cookies, and a phone stays signed in
//! across container restarts.

use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const COOKIE_NAME: &str = "tvhp_session";
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// A short human-typeable code, e.g. `K7QM-3XPD`, for gating first-run setup.
/// Avoids look-alike characters (0/O, 1/I/L).
pub fn setup_code() -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    let mut rng = rand::rngs::OsRng;
    let mut pick = || ALPHABET[(rng.next_u32() as usize) % ALPHABET.len()] as char;
    let a: String = (0..4).map(|_| pick()).collect();
    let b: String = (0..4).map(|_| pick()).collect();
    format!("{a}-{b}")
}

pub struct Sessions {
    /// digest → expiry (unix seconds)
    map: HashMap<String, u64>,
    path: Option<PathBuf>,
}

impl Sessions {
    pub fn load(path: Option<PathBuf>) -> Self {
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<HashMap<String, u64>>(&t).ok())
            .unwrap_or_default();
        let mut s = Sessions { map, path };
        s.prune();
        s
    }

    fn prune(&mut self) {
        let now = now_unix();
        self.map.retain(|_, exp| *exp > now);
    }

    fn persist(&self) {
        if let Some(p) = &self.path
            && let Ok(bytes) = serde_json::to_vec(&self.map)
            && let Err(e) = crate::config::write_atomic(p, &bytes)
        {
            eprintln!("could not save sessions: {e}");
        }
    }

    /// Create a session and return the raw token for the cookie.
    pub fn create(&mut self) -> String {
        self.prune();
        let token = random_token();
        self.map
            .insert(digest(&token), now_unix() + SESSION_TTL.as_secs());
        self.persist();
        token
    }

    pub fn is_valid(&self, token: &str) -> bool {
        self.map
            .get(&digest(token))
            .is_some_and(|exp| *exp > now_unix())
    }

    pub fn revoke(&mut self, token: &str) {
        if self.map.remove(&digest(token)).is_some() {
            self.persist();
        }
    }

    /// Sign everyone out (used after a password change).
    pub fn revoke_all_except(&mut self, keep: Option<&str>) {
        let keep = keep.map(digest);
        self.map.retain(|k, _| Some(k) == keep.as_ref());
        self.persist();
    }
}

/// Per-client failure tracking with exponential lockout: 5 free attempts, then
/// 1 min, 2 min, 4 min … capped at 1 hour. A global ceiling also slows a
/// distributed guesser that rotates addresses.
pub struct LoginLimiter {
    per_key: HashMap<String, (u32, Option<Instant>)>,
    global: Vec<Instant>,
}

pub const FREE_ATTEMPTS: u32 = 5;
const GLOBAL_WINDOW: Duration = Duration::from_secs(600);
const GLOBAL_MAX_FAILURES: usize = 50;

impl LoginLimiter {
    pub fn new() -> Self {
        LoginLimiter {
            per_key: HashMap::new(),
            global: Vec::new(),
        }
    }

    /// `Err(seconds)` while the key (or everyone) is locked out.
    pub fn check(&mut self, key: &str, now: Instant) -> Result<(), u64> {
        self.global
            .retain(|t| now.duration_since(*t) < GLOBAL_WINDOW);
        if self.global.len() >= GLOBAL_MAX_FAILURES {
            let oldest = self.global[0];
            return Err((GLOBAL_WINDOW - now.duration_since(oldest))
                .as_secs()
                .max(1));
        }
        if let Some((_, Some(until))) = self.per_key.get(key)
            && *until > now
        {
            return Err(until.duration_since(now).as_secs().max(1));
        }
        Ok(())
    }

    pub fn record_failure(&mut self, key: &str, now: Instant) {
        self.global.push(now);
        let entry = self.per_key.entry(key.to_string()).or_insert((0, None));
        entry.0 += 1;
        if entry.0 >= FREE_ATTEMPTS {
            let exp = (entry.0 - FREE_ATTEMPTS).min(6);
            entry.1 = Some(now + Duration::from_secs(60 * (1 << exp)));
        }
    }

    pub fn record_success(&mut self, key: &str) {
        self.per_key.remove(key);
    }
}

/// Pull our session token out of a `Cookie:` header value.
pub fn token_from_cookie_header(header: &str) -> Option<&str> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == COOKIE_NAME && !v.is_empty()).then_some(v)
    })
}

pub fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        SESSION_TTL.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

pub fn clear_cookie(secure: bool) -> String {
    format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}
