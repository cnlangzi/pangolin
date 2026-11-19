//! The read handlers.
//!
//! Every handler answers with the resource's JSON shape — a list is a
//! JSON array, a single resource is the object itself. There is no
//! envelope around a successful response.

use chrono::NaiveDate;

use super::{ApiResponse, Ctx};
use crate::dto::views::{
    BadbotsSettingsView, DashboardAddress, DnsProviderView, InstanceView, ProviderStatusView,
    ProvidersView, SiteOriginAllowView, SiteProviderView, SiteRateLimitBotsView, SiteRateLimitView,
    TunView, TunnelNodeView,
};
use crate::protocol::error::ApiError;

/// `GET /api/v1/instance` — what this node tells about itself.
pub async fn instance(ctx: &Ctx<'_>) -> ApiResponse {
    let runtime = {
        let db = ctx.state.app.db.lock().await;
        pangolin_core::db::get_api_config(&db).unwrap_or(None)
    };
    let (instance_id, dashboard) = match runtime {
        Some(row) => (
            row.instance_id,
            DashboardAddress {
                ip: row.dashboard_ip,
                port: row.dashboard_port.clamp(0, u16::MAX as i64) as u16,
            },
        ),
        None => (String::new(), DashboardAddress { ip: None, port: 0 }),
    };
    let tunnel_nodes = {
        let db = ctx.state.app.db.lock().await;
        pangolin_core::db::list_tuns(&db)
            .unwrap_or_default()
            .into_iter()
            .map(|t| TunnelNodeView {
                name: t.name,
                online: t.online,
                last_seen_at: t.last_seen_at,
            })
            .collect::<Vec<_>>()
    };
    let view = InstanceView {
        instance_id,
        version: pangolin_core::VERSION,
        uptime_seconds: ctx.state.app.ratelimit_snapshot().uptime_secs,
        dashboard_key_fingerprint: crate::protocol::signature::fingerprint(
            &ctx.state.authenticator.verifying_key,
        ),
        dashboard,
        tunnel_nodes,
        capabilities: super::CAPABILITIES.to_vec(),
    };
    ApiResponse::json_of(200, &view)
}

/// `GET /api/v1/sites`
pub async fn list_sites(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    ApiResponse::json_of(200, &sites)
}

/// `GET /api/v1/sites/{name}`
pub async fn get_site(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    match pangolin_core::db::get_site(&db, name).unwrap_or(None) {
        Some(site) => ApiResponse::json_of(200, &site),
        None => ApiResponse::error(&site_not_found(name)),
    }
}

/// `GET /api/v1/domains`
pub async fn list_domains(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    ApiResponse::json_of(200, &domains)
}

/// `GET /api/v1/domains/{domain}`
pub async fn get_domain(ctx: &Ctx<'_>, domain: &str) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    match pangolin_core::db::get_domain(&db, domain).unwrap_or(None) {
        Some(d) => ApiResponse::json_of(200, &d),
        None => ApiResponse::error(&ApiError::not_found(
            "domain_not_found",
            format!("no domain named '{domain}'"),
        )),
    }
}

/// `GET /api/v1/dns_providers`
pub async fn list_dns_providers(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let providers = pangolin_core::db::list_dns_providers(&db)
        .unwrap_or_default()
        .iter()
        .map(DnsProviderView::of)
        .collect::<Vec<_>>();
    ApiResponse::json_of(200, &providers)
}

/// `GET /api/v1/dns_providers/{name}`
pub async fn get_dns_provider(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    match pangolin_core::db::get_dns_provider(&db, name).unwrap_or(None) {
        Some(p) => ApiResponse::json_of(200, &DnsProviderView::of(&p)),
        None => ApiResponse::error(&ApiError::not_found(
            "dns_provider_not_found",
            format!("no DNS provider named '{name}'"),
        )),
    }
}

/// `GET /api/v1/tun`
pub async fn list_tuns(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let tuns = pangolin_core::db::list_tuns(&db)
        .unwrap_or_default()
        .iter()
        .map(TunView::of)
        .collect::<Vec<_>>();
    ApiResponse::json_of(200, &tuns)
}

/// `GET /api/v1/tun/{name}`
pub async fn get_tun(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    match pangolin_core::db::get_tun(&db, name).unwrap_or(None) {
        Some(t) => ApiResponse::json_of(200, &TunView::of(&t)),
        None => ApiResponse::error(&ApiError::not_found(
            "tun_not_found",
            format!("no tunnel named '{name}'"),
        )),
    }
}

/// `GET /api/v1/ip_rules`
pub async fn list_ip_rules(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let rules = pangolin_core::db::list_ip_rules(&db).unwrap_or_default();
    ApiResponse::json_of(200, &rules)
}

/// `GET /api/v1/site_rate_limits`
pub async fn list_site_rate_limits(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let limits = pangolin_core::db::list_site_rate_limits(&db).unwrap_or_default();
    let views = limits
        .into_iter()
        .map(|(site_name, policy)| SiteRateLimitView { site_name, policy })
        .collect::<Vec<_>>();
    ApiResponse::json_of(200, &views)
}

/// `GET /api/v1/site_rate_limits/{site}`
pub async fn get_site_rate_limit(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    match pangolin_core::db::get_site_rate_limit(&db, site).unwrap_or(None) {
        Some(policy) => ApiResponse::json_of(
            200,
            &SiteRateLimitView {
                site_name: site.to_string(),
                policy,
            },
        ),
        None => ApiResponse::error(&ApiError::not_found(
            "site_rate_limit_not_found",
            format!("no rate-limit policy for '{site}'"),
        )),
    }
}

/// `GET /api/v1/site_rate_limit_bots`
pub async fn list_site_rate_limit_bots(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let bots = pangolin_core::db::list_site_rate_limit_bots(&db).unwrap_or_default();
    let views = bots
        .into_iter()
        .map(|(site_name, bots)| SiteRateLimitBotsView {
            site_name,
            bots: bots.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    ApiResponse::json_of(200, &views)
}

/// `GET /api/v1/site_rate_limit_bots/{site}`
pub async fn get_site_rate_limit_bots(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let bots = pangolin_core::db::get_site_rate_limit_bots(&db, site).unwrap_or_default();
    ApiResponse::json_of(
        200,
        &SiteRateLimitBotsView {
            site_name: site.to_string(),
            bots: bots.into_iter().collect(),
        },
    )
}

/// `GET /api/v1/site_providers`
pub async fn list_site_providers(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let providers = pangolin_core::db::list_site_providers(&db).unwrap_or_default();
    let views = providers
        .into_iter()
        .map(|(site_name, over)| SiteProviderView::of(&site_name, &over))
        .collect::<Vec<_>>();
    ApiResponse::json_of(200, &views)
}

/// `GET /api/v1/site_providers/{site}`
pub async fn get_site_provider(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    match pangolin_core::db::get_site_provider(&db, site).unwrap_or(None) {
        Some(over) => ApiResponse::json_of(200, &SiteProviderView::of(site, &over)),
        None => ApiResponse::error(&ApiError::not_found(
            "site_provider_not_found",
            format!("no provider override for '{site}'"),
        )),
    }
}

/// `GET /api/v1/site_origin_allow`
pub async fn list_site_origin_allow(ctx: &Ctx<'_>) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let ranges = pangolin_core::db::list_site_origin_allow(&db).unwrap_or_default();
    let views = ranges
        .into_iter()
        .map(|(site_name, cidrs)| SiteOriginAllowView { site_name, cidrs })
        .collect::<Vec<_>>();
    ApiResponse::json_of(200, &views)
}

/// `GET /api/v1/site_origin_allow/{site}`
pub async fn get_site_origin_allow(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let db = ctx.state.app.db.lock().await;
    let ranges = pangolin_core::db::list_site_origin_allow(&db).unwrap_or_default();
    let cidrs = ranges.get(site).cloned().unwrap_or_default();
    ApiResponse::json_of(
        200,
        &SiteOriginAllowView {
            site_name: site.to_string(),
            cidrs,
        },
    )
}

/// `GET /api/v1/system_config`
pub async fn get_system_config(ctx: &Ctx<'_>) -> ApiResponse {
    let cfg = ctx.state.app.system_config.read().await.clone();
    ApiResponse::json_of(200, &cfg)
}

/// `GET /api/v1/badbots_config`
pub async fn get_badbots_config(ctx: &Ctx<'_>) -> ApiResponse {
    let settings = match ctx.state.app.badbots_settings().await {
        Ok(s) => s,
        Err(e) => {
            return ApiResponse::error(&ApiError::internal(format!(
                "reading the reputation configuration failed: {e}"
            )));
        }
    };
    let view = BadbotsSettingsView {
        enabled: settings.enabled,
        evidence_policy: settings.evidence_policy.as_str().to_string(),
        has_abuseipdb_api_key: settings.abuseipdb_api_key.is_some(),
        has_crowdsec_api_key: settings.crowdsec_api_key.is_some(),
        updated_at: settings.updated_at,
    };
    ApiResponse::json_of(200, &view)
}

/// `GET /api/v1/providers` — the provider registry and the state of
/// every provider's range list.
pub async fn get_providers(ctx: &Ctx<'_>) -> ApiResponse {
    // The API is a machine-facing surface: its JSON bodies stay in
    // English.
    let statuses =
        admin::routes::provider::provider_range_statuses(&ctx.state.app, admin::i18n::Lang::En)
            .await;
    let views = statuses
        .into_iter()
        .map(|s| ProviderStatusView {
            key: s.key.as_str().to_string(),
            label: pangolin_core::provider::spec(s.key).label,
            description: pangolin_core::provider::spec(s.key).description,
            range_count: s.range_count,
            fetched_at: s.fetched_at,
            status: s.status,
            last_error: s.last_error,
            stale: s.stale,
            rejections: s.rejections,
        })
        .collect::<Vec<_>>();
    let total = admin::routes::provider::total_guard_rejections(&ctx.state.app);
    ApiResponse::json_of(200, &ProvidersView::of(views, total))
}

/// `GET /api/v1/logs/bot/dates` — the dates the bot log has a file
/// for, newest first.
pub async fn bot_log_dates(ctx: &Ctx<'_>) -> ApiResponse {
    let dir = ctx.state.app.config.log.bot.dir.clone();
    let dates = pangolin_core::bot_log::list_dates(&dir).unwrap_or_default();
    let views = dates
        .into_iter()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .collect::<Vec<_>>();
    ApiResponse::json_of(200, &views)
}

/// `GET /api/v1/logs/bot/raw?date=YYYY-MM-DD` — the raw JSONL file.
pub async fn bot_log_raw(ctx: &Ctx<'_>) -> ApiResponse {
    let dir = ctx.state.app.config.log.bot.dir.clone();
    let Some(date) = ctx.query_param("date") else {
        return ApiResponse::error(&ApiError::unprocessable(
            "date_required",
            "the 'date' query parameter is missing",
            Some("date"),
        ));
    };
    let date = match parse_date(&date) {
        Some(d) => d,
        None => {
            return ApiResponse::error(&ApiError::unprocessable(
                "date_invalid",
                format!("'{date}' is not a YYYY-MM-DD date"),
                Some("date"),
            ));
        }
    };
    let path = pangolin_core::bot_log::file_path_for(&dir, date);
    raw_file(&path, "bot_log_not_found").await
}

/// `GET /api/v1/logs/referer/dates`
pub async fn referer_log_dates(ctx: &Ctx<'_>) -> ApiResponse {
    let dir = ctx.state.app.config.log.referer.dir.clone();
    let dates = pangolin_core::referer_log::list_dates(&dir).unwrap_or_default();
    let views = dates
        .into_iter()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .collect::<Vec<_>>();
    ApiResponse::json_of(200, &views)
}

/// `GET /api/v1/logs/referer/raw?date=YYYY-MM-DD`
pub async fn referer_log_raw(ctx: &Ctx<'_>) -> ApiResponse {
    let dir = ctx.state.app.config.log.referer.dir.clone();
    let Some(date) = ctx.query_param("date") else {
        return ApiResponse::error(&ApiError::unprocessable(
            "date_required",
            "the 'date' query parameter is missing",
            Some("date"),
        ));
    };
    let date = match parse_date(&date) {
        Some(d) => d,
        None => {
            return ApiResponse::error(&ApiError::unprocessable(
                "date_invalid",
                format!("'{date}' is not a YYYY-MM-DD date"),
                Some("date"),
            ));
        }
    };
    let path = pangolin_core::referer_log::file_path_for(&dir, date);
    raw_file(&path, "referer_log_not_found").await
}

/// `GET /api/v1/logs/access` — the in-memory access log.
pub async fn access_log(ctx: &Ctx<'_>) -> ApiResponse {
    let entries = ctx.state.app.recent_access_log();
    ApiResponse::json_of(200, &entries)
}

/// Read a raw log file, byte for byte.
async fn raw_file(path: &std::path::Path, not_found_code: &str) -> ApiResponse {
    match tokio::fs::read(path).await {
        Ok(bytes) => ApiResponse::text(200, bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ApiResponse::error(&ApiError::not_found(
                not_found_code,
                format!("no log file for '{}'", path.display()),
            ))
        }
        Err(e) => ApiResponse::error(&ApiError::internal(format!(
            "reading the log file failed: {e}"
        ))),
    }
}

/// Parse a `YYYY-MM-DD` date.
fn parse_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok()
}

/// The 404 of a missing site.
pub fn site_not_found(name: &str) -> ApiError {
    ApiError::not_found("site_not_found", format!("no site named '{name}'"))
}

/// The 409 of an existing site.
pub fn site_exists(name: &str) -> ApiError {
    ApiError::conflict(
        "site_exists",
        format!("a site named '{name}' already exists"),
    )
}

/// The 409 of an existing domain.
pub fn domain_exists(domain: &str) -> ApiError {
    ApiError::conflict(
        "domain_exists",
        format!("a domain named '{domain}' already exists"),
    )
}

/// The 409 of an existing DNS provider.
pub fn dns_provider_exists(name: &str) -> ApiError {
    ApiError::conflict(
        "dns_provider_exists",
        format!("a DNS provider named '{name}' already exists"),
    )
}

/// The 409 of an existing tunnel.
pub fn tun_exists(name: &str) -> ApiError {
    ApiError::conflict(
        "tun_exists",
        format!("a tunnel named '{name}' already exists"),
    )
}

/// The error of a failed database write.
pub fn database_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(format!("the database write failed: {e}"))
}
