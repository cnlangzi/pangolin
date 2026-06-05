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

use bytes::Bytes;
use chrono::Utc;
use http::Response;
use log::{debug, warn};
use pingora::http::ResponseHeader;
use pingora::protocols::http::ServerSession;
use pingora::proxy::Session;

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
    let site = Site {
        name: name.to_string(),
        backend: req.backend,
        enabled: req.enabled.unwrap_or(true),
        created_at: now,
        updated_at: now,
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

async fn delete_site(app: &App, name: &str) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::delete_site(&conn, name) {
        Ok(true) => {
            drop(conn);
            app.reload_indexes().await;
            debug!("deleted site: {}", name);
            json_ok(b"{\"ok\":true}".as_slice())
        }
        Ok(false) => json_error(404, "site not found"),
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

async fn delete_domain(app: &App, domain: &str) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::delete_domain(&conn, domain) {
        Ok(true) => {
            drop(conn);
            app.reload_indexes().await;
            debug!("deleted domain: {}", domain);
            json_ok(b"{\"ok\":true}".as_slice())
        }
        Ok(false) => json_error(404, "domain not found"),
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

async fn delete_tun(app: &App, name: &str) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::delete_tun(&conn, name) {
        Ok(true) => {
            drop(conn);
            app.reload_indexes().await;
            debug!("deleted tun: {}", name);
            json_ok(b"{\"ok\":true}".as_slice())
        }
        Ok(false) => json_error(404, "tun not found"),
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

async fn delete_token(app: &App, token: &str) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::delete_token(&conn, token) {
        Ok(true) => {
            drop(conn);
            app.reload_indexes().await;
            debug!("deleted token: {}", token);
            json_ok(b"{\"ok\":true}".as_slice())
        }
        Ok(false) => json_error(404, "token not found"),
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
    json_ok(serde_json::to_vec(&events).unwrap().as_slice())
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
    json_ok(serde_json::to_vec(&settings).unwrap().as_slice())
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
    json_ok(serde_json::to_vec(&settings).unwrap().as_slice())
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

async fn delete_cert(app: &App, domain: &str) -> Response<Vec<u8>> {
    let conn = app.db.lock().await;
    match db::delete_cert(&conn, domain) {
        Ok(true) => {
            debug!("deleted cert: {}", domain);
            json_ok(b"{\"ok\":true}".as_slice())
        }
        Ok(false) => json_error(404, "cert not found"),
        Err(e) => json_error(500, &format!("db error: {}", e)),
    }
}

// ---- Main router for proxy session (PUT/DELETE :name paths) ----

/// Handle a REST API request from the proxy path (PUT/DELETE with :name in path).
pub async fn handle_api_request(
    session: &mut Session,
    app: &App,
    path: &str,
    method: &str,
) -> pingora::Result<()> {
    let parts: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();
    if parts.len() < 2 {
        let _ = session.respond_error(400).await;
        return Ok(());
    }

    let resource = parts[0];
    let name = parts[1];
    let body = match session.read_body_or_idle(false).await {
        Ok(Some(b)) => b.to_vec(),
        Ok(None) => vec![],
        Err(_) => {
            let _ = session.respond_error(400).await;
            return Ok(());
        }
    };

    match (resource, method) {
        ("sites", "PUT") => {
            let resp = upsert_site(app, name, &body).await;
            write_json_response(session, resp).await;
        }
        ("sites", "DELETE") => {
            let resp = delete_site(app, name).await;
            write_json_response(session, resp).await;
        }
        ("domains", "PUT") => {
            let resp = upsert_domain(app, name, &body).await;
            write_json_response(session, resp).await;
        }
        ("domains", "DELETE") => {
            let resp = delete_domain(app, name).await;
            write_json_response(session, resp).await;
        }
        ("tun", "PUT") => {
            let resp = upsert_tun(app, name, &body).await;
            write_json_response(session, resp).await;
        }
        ("tun", "DELETE") => {
            let resp = delete_tun(app, name).await;
            write_json_response(session, resp).await;
        }
        ("tokens", "PUT") => {
            let resp = upsert_token(app, name, &body).await;
            write_json_response(session, resp).await;
        }
        ("tokens", "DELETE") => {
            let resp = delete_token(app, name).await;
            write_json_response(session, resp).await;
        }
        ("certs", "DELETE") => {
            let resp = delete_cert(app, name).await;
            write_json_response(session, resp).await;
        }
        _ => {
            let _ = session.respond_error(404).await;
        }
    }
    Ok(())
}

/// Handle a REST API request from the HTTP server (GET/POST with collection path).
pub async fn handle_api_http(
    _http_session: &mut ServerSession,
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
                update_cert_settings(app, &[]).await
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

// ---- Response writers ----

async fn write_json_response(session: &mut Session, resp: Response<Vec<u8>>) {
    let status = resp.status().as_u16();
    let body = resp.body().clone();
    let content_type = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .unwrap_or("application/json");

    let mut hdr = match ResponseHeader::build(status, None) {
        Ok(h) => h,
        Err(e) => {
            log::error!("failed to build response header: {}", e);
            return;
        }
    };
    hdr.insert_header("Content-Type", content_type.as_bytes())
        .ok();

    if let Err(e) = session.write_response_header(Box::new(hdr), true).await {
        log::error!("failed to write response header: {}", e);
        return;
    }
    if let Err(e) = session
        .write_response_body(Some(Bytes::from(body)), true)
        .await
    {
        log::error!("failed to write response body: {}", e);
    }
}
