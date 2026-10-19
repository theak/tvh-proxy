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
/// 400 days is the longest cookie lifetime browsers honour. Sessions renew as they're
/// used (see `Sessions::renew`), so a device that keeps watching never signs in again.
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 400);
const RENEW_AFTER: Duration = Duration::from_secs(60 * 60 * 24);

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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct SessionEntry {
    exp: u64,
    /// Account id. Empty for sessions created before accounts existed; those belong
    /// to the admin (resolved by the caller).
    #[serde(default)]
    account: String,
}

/// sessions.json holds `{digest: {exp, account}}`, or `{digest: exp}` from older versions.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum StoredEntry {
    Current(SessionEntry),
    Legacy(u64),
}

pub struct Sessions {
    map: HashMap<String, SessionEntry>,
    path: Option<PathBuf>,
}

impl Sessions {
    pub fn load(path: Option<PathBuf>) -> Self {
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<HashMap<String, StoredEntry>>(&t).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                let e = match v {
                    StoredEntry::Current(e) => e,
                    StoredEntry::Legacy(exp) => SessionEntry {
                        exp,
                        account: String::new(),
                    },
                };
                (k, e)
            })
            .collect();
        let mut s = Sessions { map, path };
        s.prune();
        s
    }

    fn prune(&mut self) {
        let now = now_unix();
        self.map.retain(|_, e| e.exp > now);
    }

    fn persist(&self) {
        if let Some(p) = &self.path
            && let Ok(bytes) = serde_json::to_vec(&self.map)
            && let Err(e) = crate::config::write_atomic(p, &bytes)
        {
            eprintln!("could not save sessions: {e}");
        }
    }

    /// Create a session for `account` and return the raw token for the cookie.
    pub fn create(&mut self, account: &str) -> String {
        self.prune();
        let token = random_token();
        self.map.insert(
            digest(&token),
            SessionEntry {
                exp: now_unix() + SESSION_TTL.as_secs(),
                account: account.to_string(),
            },
        );
        self.persist();
        token
    }

    /// The account id behind a live session (`""` for a pre-accounts session).
    pub fn account_for(&self, token: &str) -> Option<String> {
        self.map
            .get(&digest(token))
            .filter(|e| e.exp > now_unix())
            .map(|e| e.account.clone())
    }

    /// Push a live session's expiry back out to the full TTL, at most once a day.
    /// True when renewed, meaning the caller should re-send the cookie.
    pub fn renew(&mut self, token: &str) -> bool {
        let now = now_unix();
        let Some(e) = self.map.get_mut(&digest(token)) else {
            return false;
        };
        if e.exp <= now || e.exp > now + SESSION_TTL.as_secs() - RENEW_AFTER.as_secs() {
            return false;
        }
        e.exp = now + SESSION_TTL.as_secs();
        self.persist();
        true
    }

    pub fn is_valid(&self, token: &str) -> bool {
        self.account_for(token).is_some()
    }

    pub fn revoke(&mut self, token: &str) {
        if self.map.remove(&digest(token)).is_some() {
            self.persist();
        }
    }

    /// Sign an account out everywhere, optionally keeping one session (the caller's).
    /// `legacy_too` also drops pre-accounts sessions (they belong to the admin).
    pub fn revoke_account(&mut self, account: &str, keep: Option<&str>, legacy_too: bool) {
        let keep = keep.map(digest);
        self.map.retain(|k, e| {
            let mine = e.account == account || (legacy_too && e.account.is_empty());
            !mine || Some(k) == keep.as_ref()
        });
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
