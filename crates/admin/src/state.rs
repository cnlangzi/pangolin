//! Admin UI — session state + auth middleware.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cookie::CookieJar;
use rand::Rng;
use tokio::sync::RwLock;

/// In-memory session store. Key = secure random token, Value = expiry instant.
/// Sessions survive process restarts? No — by design (进程内可用).
#[derive(Default)]
pub struct SessionStore {
    /// token → expiry
    sessions: RwLock<HashMap<String, Instant>>,
}

impl SessionStore {
    /// Create a new session for the given username. Returns the session token.
    pub async fn create_session(&self, _username: &str) -> String {
        let token = generate_token();
        let expiry = Instant::now() + Duration::from_secs(86400); // 24h
        self.sessions.write().await.insert(token.clone(), expiry);
        token
    }

    /// Validate a token. Returns true if valid and not expired.
    pub fn is_valid(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        // Use blocking read inside async context via try_read()
        let sessions = self.sessions.blocking_read();
        match sessions.get(token) {
            Some(expiry) => Instant::now() < *expiry,
            None => false,
        }
    }

    /// Validate a token (async version, same logic).
    pub async fn validate(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let sessions = self.sessions.read().await;
        match sessions.get(token) {
            Some(expiry) => Instant::now() < *expiry,
            None => false,
        }
    }

    /// Delete a session (logout).
    pub async fn destroy(&self, token: &str) {
        self.sessions.write().await.remove(token);
    }

    /// Clean up expired sessions. Called periodically.
    pub async fn cleanup(&self) {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, expiry| now < *expiry);
    }
}

/// Generate a cryptographically weak but adequate random session token.
/// 32 bytes, hex-encoded.
fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 32] = rng.gen();
    hex::encode(bytes)
}

/// Parse session token from a Cookie header string.
pub fn parse_session_cookie(cookie_header: &str) -> Option<String> {
    let _jar = CookieJar::default();
    // Cookie::parse requires a full cookie string "name=value"
    // We can't easily use the jar API here without Accessor key.
    // Simple manual parse instead.
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once('=') {
            if key.trim() == "pangolin_session" {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Build a Set-Cookie header value for the session token.
pub fn make_session_cookie(token: &str) -> String {
    format!(
        "pangolin_session={}; HttpOnly; Path=/admin; SameSite=Strict; Max-Age=86400",
        token
    )
}

/// Build a Set-Cookie header that expires the session immediately.
pub fn make_logout_cookie() -> String {
    "pangolin_session=; HttpOnly; Path=/admin; SameSite=Strict; Max-Age=0".to_string()
}