//! Admin UI — session state + auth middleware.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::Rng;
use tokio::sync::RwLock;

/// In-memory session store. Key = session token, Value = SessionData.
/// Sessions are process-local; restart clears them.
#[derive(Default)]
pub struct SessionStore {
    sessions: RwLock<HashMap<String, SessionData>>,
}

#[derive(Clone)]
struct SessionData {
    /// Expiry instant
    expiry: Instant,
    /// CSRF token for this session (separate from session token)
    csrf: String,
    /// Username for audit logging
    username: String,
}

impl SessionStore {
    /// Create a new session. Returns (session_token, csrf_token).
    pub async fn create_session(&self, username: &str) -> (String, String) {
        let token = generate_token();
        let csrf = generate_token();
        let expiry = Instant::now() + Duration::from_secs(86400); // 24h
        self.sessions.write().await.insert(
            token.clone(),
            SessionData {
                expiry,
                csrf: csrf.clone(),
                username: username.to_string(),
            },
        );
        (token, csrf)
    }

    /// Validate a session token. Returns true if valid and not expired.
    pub async fn validate(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let sessions = self.sessions.read().await;
        match sessions.get(token) {
            Some(data) => Instant::now() < data.expiry,
            None => false,
        }
    }

    /// Look up the CSRF token for a session.
    pub async fn csrf_for(&self, session_token: &str) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions.get(session_token).map(|d| d.csrf.clone())
    }

    /// Verify a CSRF token against a session.
    /// Uses constant-time comparison to prevent timing attacks.
    pub async fn verify_csrf(&self, session_token: &str, provided_csrf: &str) -> bool {
        if session_token.is_empty() || provided_csrf.is_empty() {
            return false;
        }
        let sessions = self.sessions.read().await;
        let Some(data) = sessions.get(session_token) else {
            return false;
        };
        if Instant::now() >= data.expiry {
            return false;
        }
        // Constant-time string comparison
        if data.csrf.len() != provided_csrf.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in data.csrf.bytes().zip(provided_csrf.bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// Delete a session (logout).
    pub async fn destroy(&self, token: &str) {
        self.sessions.write().await.remove(token);
    }

    /// Clean up expired sessions. Called periodically.
    pub async fn cleanup(&self) {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, data| now < data.expiry);
    }
}

/// Generate a cryptographically random 32-byte hex-encoded token.
fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 32] = rng.gen();
    hex::encode(bytes)
}

/// Parse the session token from a Cookie header string.
pub fn parse_session_cookie(cookie_header: &str) -> Option<String> {
    parse_cookie_named(cookie_header, "pangolin_session")
}

/// Parse the CSRF token from a Cookie header string.
pub fn parse_csrf_cookie(cookie_header: &str) -> Option<String> {
    parse_cookie_named(cookie_header, "pangolin_csrf")
}

fn parse_cookie_named(cookie_header: &str, name: &str) -> Option<String> {
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once('=') {
            if key.trim() == name {
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

/// Build a Set-Cookie header for the CSRF token.
/// NOT HttpOnly so server-rendered forms can include the value.
/// The token is also embedded in the rendered HTML form, so JS access is a defense in depth.
pub fn make_csrf_cookie(csrf: &str) -> String {
    format!(
        "pangolin_csrf={}; Path=/admin; SameSite=Strict; Max-Age=86400",
        csrf
    )
}

/// Build a Set-Cookie header that expires the session immediately.
pub fn make_logout_cookie() -> String {
    "pangolin_session=; HttpOnly; Path=/admin; SameSite=Strict; Max-Age=0".to_string()
}

/// Build a Set-Cookie header that expires the CSRF cookie.
pub fn make_logout_csrf_cookie() -> String {
    "pangolin_csrf=; Path=/admin; SameSite=Strict; Max-Age=0".to_string()
}