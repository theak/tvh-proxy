//! Persisted settings (`$DATA_DIR/config.json`), written once by first-run setup and
//! edited later from the settings sheet. The password is only ever stored as an
//! argon2 hash; it is never echoed back to any client.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_VIEWERS_LIMIT: u32 = 8;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Base URL of tvheadend, e.g. `http://192.168.0.149:9981` (no trailing slash).
    pub tvh_url: String,
    /// Optional tvheadend credentials, for installs that don't allow anonymous access.
    #[serde(default)]
    pub tvh_username: String,
    #[serde(default)]
    pub tvh_password: String,
    pub username: String,
    pub password_hash: String,
    /// Lowercased, trimmed, in priority order.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// How many distinct streams (each its own channel) may run at once.
    pub max_viewers: u32,
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
    match serde_json::from_str(&text) {
        Ok(s) => Some(s),
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
