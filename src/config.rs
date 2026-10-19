//! Persisted settings (`$DATA_DIR/config.json`), written once by first-run setup and
//! edited later by the admin on the settings page. Passwords are only ever stored as
//! argon2 hashes; they are never echoed back to any client.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_VIEWERS_LIMIT: u32 = 8;

/// A sign-in. The account created by first-run setup is the admin; only admins see
/// settings. At most one account is the priority viewer (see `stream::Hub`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub priority: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Base URL of tvheadend, e.g. `http://192.168.0.149:9981` (no trailing slash).
    pub tvh_url: String,
    /// Optional tvheadend credentials, for installs that don't allow anonymous access.
    #[serde(default)]
    pub tvh_username: String,
    #[serde(default)]
    pub tvh_password: String,
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Single-login configs from before accounts existed; migrated into an admin
    /// account on load and never written back.
    #[serde(default, skip_serializing)]
    pub username: Option<String>,
    #[serde(default, skip_serializing)]
    pub password_hash: Option<String>,
    /// Lowercased, trimmed, in priority order.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// How many distinct streams (each its own channel) may run at once.
    pub max_viewers: u32,
}

impl Settings {
    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn account_by_name(&self, username: &str) -> Option<&Account> {
        let u = username.trim();
        self.accounts
            .iter()
            .find(|a| a.username.eq_ignore_ascii_case(u))
    }

    pub fn first_admin(&self) -> Option<&Account> {
        self.accounts.iter().find(|a| a.admin)
    }

    /// Make `id` the only priority account (or clear priority with `None`).
    pub fn set_priority(&mut self, id: Option<&str>) {
        for a in &mut self.accounts {
            a.priority = Some(a.id.as_str()) == id;
        }
    }

    /// Fold a pre-accounts config into an admin account.
    fn migrate(&mut self) {
        if self.accounts.is_empty()
            && let (Some(u), Some(h)) = (self.username.take(), self.password_hash.take())
        {
            self.accounts.push(Account {
                id: new_account_id(),
                username: u,
                password_hash: h,
                admin: true,
                priority: false,
            });
        }
        self.username = None;
        self.password_hash = None;
    }
}

pub fn new_account_id() -> String {
    crate::auth::random_token()[..12].to_string()
}

/// Usernames are case-insensitively unique and kept short and printable.
pub fn validate_new_username(settings: &Settings, username: &str) -> Result<String, String> {
    let u = username.trim();
    if u.is_empty() {
        return Err("Choose a username.".into());
    }
    if u.chars().count() > 40 || u.chars().any(char::is_control) {
        return Err("Usernames can be at most 40 characters.".into());
    }
    if settings.account_by_name(u).is_some() {
        return Err(format!("There's already an account called {u}."));
    }
    Ok(u.to_string())
}

/// Normalize a user-entered tvheadend address: add `http://` if missing, drop
/// trailing slashes. Returns `None` when it doesn't look like a URL at all.
pub fn normalize_tvh_url(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    let (scheme, rest) = match s.split_once("://") {
        Some((sch, rest)) if sch == "http" || sch == "https" => (sch, rest),
        Some(_) => return None,
        None => ("http", s),
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.starts_with(':') || rest.starts_with('/') {
        return None;
    }
    Some(format!("{scheme}://{rest}"))
}

/// Split "nfl, Olympics ,," into `["nfl", "olympics"]`, preserving order, deduped.
pub fn parse_keywords(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in raw.split(',') {
        let k = k.trim().to_lowercase();
        if !k.is_empty() && !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

pub fn validate_credentials(username: &str, password: &str, confirm: &str) -> Result<(), String> {
    if username.trim().is_empty() {
        return Err("Choose a username.".into());
    }
    if password.chars().count() < 8 {
        return Err("Password must be at least 8 characters.".into());
    }
    if password != confirm {
        return Err("Passwords don't match.".into());
    }
    Ok(())
}

pub fn validate_max_viewers(n: u32) -> Result<u32, String> {
    if (1..=MAX_VIEWERS_LIMIT).contains(&n) {
        Ok(n)
    } else {
        Err(format!(
            "Max viewers must be between 1 and {MAX_VIEWERS_LIMIT}."
        ))
    }
}

pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hashing cannot fail with a generated salt")
        .to_string()
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
        .unwrap_or(false)
}

pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.json")
}

pub fn load(data_dir: &Path) -> Option<Settings> {
    let text = std::fs::read_to_string(config_path(data_dir)).ok()?;
    match serde_json::from_str::<Settings>(&text) {
        Ok(mut s) => {
            let legacy = s.accounts.is_empty();
            s.migrate();
            if legacy && !s.accounts.is_empty() {
                // Persist the migration so account ids stay stable across restarts.
                if let Err(e) = save(data_dir, &s) {
                    eprintln!("could not save migrated config: {e}");
                }
            }
            Some(s)
        }
        Err(e) => {
            eprintln!("config.json is unreadable ({e}); starting first-run setup");
            None
        }
    }
}

/// Write atomically (temp file + rename) so a crash never leaves a half-written config.
pub fn save(data_dir: &Path, settings: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    write_atomic(
        &config_path(data_dir),
        &serde_json::to_vec_pretty(settings)?,
    )
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)
}
