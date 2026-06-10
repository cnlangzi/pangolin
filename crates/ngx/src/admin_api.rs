//! Admin REST API handler.
//!
//! All endpoints return JSON. Writes trigger a reload of in-memory indexes.
//!
//! Routes:
//!   GET/POST   /api/sites
//!   PUT/DELETE /api/sites/:name
//!   GET/POST   /api/domains
//!   PUT/DELETE /api/domains/:domain
//!   GET/POST   /api/tun
//!   PUT/DELETE /api/tun/:name
//!   GET/POST   /api/tokens
//!   PUT/DELETE /api/tokens/:token
//!   GET/POST   /api/certs
//!   DELETE     /api/certs/:domain
//!   GET        /api/events

use chrono::Utc;
use http::Response;
use log::{debug, warn};
use pingora::protocols::http::ServerSession;

use crate::App;
use pangolin_core::{
    db, is_valid_domain, is_valid_tun_name, parse_backend,
    types::{Cert, Domain, Site, Token, Tun},
};

// ---- JSON response helpers ----

fn json_response(status: u16, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(body)
        .unwrap()
}

fn json_ok(body: &[u8]) -> Response<Vec<u8>> {
    json_response(200, body.to_vec())
}

fn json_created(body: &[u8]) -> Response<Vec<u8>> {
    json_response(201, body.to_vec())
}

fn json_error(status: u16, message: &str) -> Response<Vec<u8>> {
    json_response(
        status,
        serde_json::to_vec(&serde_json::json!({"error": message})).unwrap(),
    )
}

// ---- Read body helper ----

#[allow(dead_code)]
async fn read_body_http(http_session: &mut ServerSession) -> Result<Vec<u8>, Response<Vec<u8>>> {
    match http_session.read_body_or_idle(false).await {
        Ok(Some(data)) => Ok(data.to_vec()),
        Ok(None) => Ok(vec![]),
        Err(e) => {
            warn!("Failed to read request body: {}", e);
            Err(json_error(400, "Failed to read request body"))
        }
    }
}

// ---- Sites ----

async fn list_sites(app: &App) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::list_sites(&conn) {
        Ok(sites) => json_ok(serde_json::to_vec(&sites).unwrap().as_slice()),
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

async fn upsert_site(app: &App, name: &str, body: &[u8]) -> Response<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Req {
        backend: String,
        enabled: Option<bool>,
        host_mode: Option<String>,
        host_custom: Option<String>,
    }
    let req: Req = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return json_error(400, &format!("invalid JSON: {}", e)),
    };

    // Validate backend at upsert time
    if let Err(e) = parse_backend(&req.backend) {
        return json_error(400, &format!("invalid backend: {}", e));
    }

    let now = Utc::now();
    let host_mode = match req.host_mode.as_deref().unwrap_or("passthrough").parse() {
        Ok(mode) => mode,
        Err(_) => {
            return json_error(
                400,
                "invalid value for `host_mode` (expected: backend, passthrough, custom)",
            );
        }
    };
    let site = Site {
        name: name.to_string(),
        backend: req.backend,
        enabled: req.enabled.unwrap_or(true),
        created_at: now,
        updated_at: now,
        host_mode,
        host_custom: req.host_custom,
        domain_count: 0,
    };

    let conn = app.db.lock().await;
    match db::upsert_site(&conn, &site) {
        Ok(()) => {
            drop(conn);
            app.reload_indexes().await;
            debug!("upserted site: {}", name);
            json_created(serde_json::to_vec(&site).unwrap().as_slice())
        }
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

// ---- Domains ----

async fn list_domains(app: &App) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::list_domains(&conn) {
        Ok(domains) => json_ok(serde_json::to_vec(&domains).unwrap().as_slice()),
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

async fn upsert_domain(app: &App, domain: &str, body: &[u8]) -> Response<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Req {
        site_name: String,
        enabled: Option<bool>,
    }
    let req: Req = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return json_error(400, &format!("invalid JSON: {}", e)),
    };

    if !is_valid_domain(domain) {
        return json_error(400, "invalid domain");
    }

    let d = Domain {
        domain: domain.to_string(),
        site_name: req.site_name,
        enabled: req.enabled.unwrap_or(true),
        created_at: Utc::now(),
    };

    let conn = app.db.lock().await;
    match db::upsert_domain(&conn, &d) {
        Ok(()) => {
            drop(conn);
            app.reload_indexes().await;
            debug!("upserted domain: {}", domain);
            json_created(serde_json::to_vec(&d).unwrap().as_slice())
        }
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

// ---- Tun ----

async fn list_tuns(app: &App) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::list_tuns(&conn) {
        Ok(tuns) => json_ok(serde_json::to_vec(&tuns).unwrap().as_slice()),
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

async fn upsert_tun(app: &App, name: &str, body: &[u8]) -> Response<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Req {
        enabled: Option<bool>,
        online: Option<bool>,
    }
    let req: Req = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return json_error(400, &format!("invalid JSON: {}", e)),
    };

    if !is_valid_tun_name(name) {
        return json_error(400, "invalid tun name");
    }

    let t = Tun {
        name: name.to_string(),
        enabled: req.enabled.unwrap_or(true),
        online: req.online.unwrap_or(false),
        registered_at: None,
        last_seen_at: None,
    };

    let conn = app.db.lock().await;
    match db::upsert_tun(&conn, &t) {
        Ok(()) => {
            drop(conn);
            app.reload_indexes().await;
            debug!("upserted tun: {}", name);
            json_created(serde_json::to_vec(&t).unwrap().as_slice())
        }
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

// ---- Tokens ----

async fn list_tokens(app: &App) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::list_tokens(&conn) {
        Ok(tokens) => json_ok(serde_json::to_vec(&tokens).unwrap().as_slice()),
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

async fn upsert_token(app: &App, token: &str, body: &[u8]) -> Response<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Req {
        enabled: Option<bool>,
        expires_at: Option<String>,
    }
    let req: Req = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return json_error(400, &format!("invalid JSON: {}", e)),
    };

    let expires_at = match &req.expires_at {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc)),
        None => None,
    };

    let t = Token {
        token: token.to_string(),
        enabled: req.enabled.unwrap_or(true),
        created_at: Utc::now(),
        expires_at,
    };

    let conn = app.db.lock().await;
    match db::upsert_token(&conn, &t) {
        Ok(()) => {
            drop(conn);
            app.reload_indexes().await;
            debug!("upserted token: {}", token);
            json_created(serde_json::to_vec(&t).unwrap().as_slice())
        }
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

// ---- Certs ----

async fn list_certs(app: &App) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::list_certs(&conn) {
        Ok(certs) => json_ok(serde_json::to_vec(&certs).unwrap().as_slice()),
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

// ---- Events ----

async fn list_events(app: &App) -> Response<Vec<u8>> {
    let events = app.get_recent_events(20);
    let body = match serde_json::to_vec(&events) {
        Ok(b) => b,
        Err(e) => return json_error(500, &format!("failed to serialize events: {}", e)),
    };
    json_ok(body.as_slice())
}

// ---- Cert Settings ----

#[derive(serde::Serialize)]
struct CertSettings {
    autorenew_enabled: bool,
    autorenew_override: Option<bool>,
}

async fn get_cert_settings(app: &App) -> Response<Vec<u8>> {
    let settings = CertSettings {
        autorenew_enabled: app.cert_manager.is_autorenew_enabled(),
        autorenew_override: app.cert_manager.get_autorenew_setting(),
    };
    let body = match serde_json::to_vec(&settings) {
        Ok(b) => b,
        Err(e) => return json_error(500, &format!("failed to serialize settings: {}", e)),
    };
    json_ok(body.as_slice())
}

async fn update_cert_settings(app: &App, body: &[u8]) -> Response<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Req {
        autorenew: Option<bool>,
    }
    let req: Req = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return json_error(400, &format!("invalid JSON: {}", e)),
    };

    // Set the runtime override. If autorenew is Some(bool), it overrides the config.
    // If autorenew is None, we clear the override (use config value).
    app.cert_manager.set_autorenew_override(req.autorenew);

    let settings = CertSettings {
        autorenew_enabled: app.cert_manager.is_autorenew_enabled(),
        autorenew_override: app.cert_manager.get_autorenew_setting(),
    };
    let body = match serde_json::to_vec(&settings) {
        Ok(b) => b,
        Err(e) => return json_error(500, &format!("failed to serialize settings: {}", e)),
    };
    json_ok(body.as_slice())
}

async fn upsert_cert(app: &App, domain: &str, body: &[u8]) -> Response<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Req {
        cert_file: String,
        key_file: String,
        expires_at: Option<String>,
    }
    let req: Req = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return json_error(400, &format!("invalid JSON: {}", e)),
    };

    let expires_at = match &req.expires_at {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc)),
        None => None,
    };

    let c = Cert {
        domain: domain.to_string(),
        cert_file: req.cert_file,
        key_file: req.key_file,
        expires_at,
        created_at: Utc::now(),
    };

    let conn = app.db.lock().await;
    match db::upsert_cert(&conn, &c) {
        Ok(()) => {
            debug!("upserted cert: {}", domain);
            json_created(serde_json::to_vec(&c).unwrap().as_slice())
        }
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

/// Handle a REST API request from the HTTP server (GET/POST with collection path).
pub async fn handle_api_http(
    http_session: &mut ServerSession,
    app: &App,
    path: &str,
    method: &str,
) -> Response<Vec<u8>> {
    let rel = path.trim_start_matches("/api/").trim_start_matches('/');
    let parts: Vec<&str> = rel.split('/').collect();

    match (parts[0], method) {
        // Sites
        ("sites", "GET") => list_sites(app).await,
        ("sites", "POST") => {
            if parts.len() >= 2 {
                upsert_site(app, parts[1], &[]).await
            } else {
                json_error(400, "POST requires site name in path")
            }
        }
        // Domains
        ("domains", "GET") => list_domains(app).await,
        ("domains", "POST") => {
            if parts.len() >= 2 {
                upsert_domain(app, parts[1], &[]).await
            } else {
                json_error(400, "POST requires domain in path")
            }
        }
        // Tun
        ("tun", "GET") => list_tuns(app).await,
        ("tun", "POST") => {
            if parts.len() >= 2 {
                upsert_tun(app, parts[1], &[]).await
            } else {
                json_error(400, "POST requires tun name in path")
            }
        }
        // Tokens
        ("tokens", "GET") => list_tokens(app).await,
        ("tokens", "POST") => {
            if parts.len() >= 2 {
                upsert_token(app, parts[1], &[]).await
            } else {
                json_error(400, "POST requires token in path")
            }
        }
        // Certs
        ("certs", "GET") => {
            if parts.len() >= 2 && parts[1] == "settings" {
                get_cert_settings(app).await
            } else {
                list_certs(app).await
            }
        }
        ("certs", "PUT") => {
            if parts.len() >= 2 && parts[1] == "settings" {
                let body = match read_body_http(http_session).await {
                    Ok(b) => b,
                    Err(resp) => return resp,
                };
                update_cert_settings(app, &body).await
            } else {
                json_error(400, "PUT requires /api/certs/settings")
            }
        }
        ("certs", "POST") => {
            if parts.len() >= 2 {
                upsert_cert(app, parts[1], &[]).await
            } else {
                json_error(400, "POST requires domain in path")
            }
        }
        // Events
        ("events", "GET") => list_events(app).await,
        _ => json_error(404, "not found"),
    }
}
