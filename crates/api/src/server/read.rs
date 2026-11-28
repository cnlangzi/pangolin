//! The read handlers.
//!
//! Every handler answers with the resource's JSON shape — a list is a
//! JSON array, a single resource is the object itself. There is no
//! envelope around a successful response.

use chrono::NaiveDate;

use super::paging::Page;
use super::{ApiResponse, Ctx};
use crate::dto::views::{
    BadbotsSettingsView, DnsProviderView, InstanceView, ProviderStatusView, ProvidersView,
    SiteOriginAllowView, SiteProviderView, SiteRateLimitBotsView, SiteRateLimitView, TunView,
    TunnelNodeView,
};
use crate::protocol::error::ApiError;

/// `GET /api/v1/instance` — what this node tells about itself.
pub async fn instance(ctx: &Ctx<'_>) -> ApiResponse {
    // The reads run on the blocking pool: they are synchronous
    // SQLite calls, and a worker thread that runs one is a worker
    // thread that is not proxying requests (issue #254).
    let runtime = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::get_api_config(db).unwrap_or(None))
        .await;
    // The node dials the dashboard, so the address the dashboard has
    // to reach is the one the node dials **out** to — the dash API.
    let (instance_id, dash_api) = match runtime {
        Some(row) => {
            // The effective address is read before the row is moved.
            let dash_api = row.effective_dash_api().to_string();
            (row.instance_id, dash_api)
        }
        None => (
            String::new(),
            pangolin_core::db::DEFAULT_DASH_API.to_string(),
        ),
    };
    let tunnel_nodes = ctx
        .state
        .app
        .with_db_or(|db| {
            pangolin_core::db::list_tuns(db)
                .unwrap_or_default()
                .into_iter()
                .map(|t| TunnelNodeView {
                    name: t.name,
                    online: t.online,
                    last_seen_at: t.last_seen_at,
                })
                .collect::<Vec<_>>()
        })
        .await;
    let view = InstanceView {
        instance_id,
        version: pangolin_core::VERSION,
        uptime_seconds: ctx.state.app.ratelimit_snapshot().uptime_secs,
        dashboard_key_fingerprint: ctx.state.dashboard_key.fingerprint(),
        dash_api,
        tunnel_nodes,
        capabilities: super::CAPABILITIES.to_vec(),
    };
    ApiResponse::json_of(200, &view)
}

/// `GET /api/v1/sites`
pub async fn list_sites(ctx: &Ctx<'_>) -> ApiResponse {
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let sites = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::list_sites(db).unwrap_or_default())
        .await;
    let items = page.slice(&sites);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/sites/{name}`
pub async fn get_site(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let key = name.to_string();
    let site = ctx
        .state
        .app
        .with_db_or(move |db| pangolin_core::db::get_site(db, &key).unwrap_or(None))
        .await;
    match site {
        Some(site) => ApiResponse::json_of(200, &site),
        None => ApiResponse::error(&site_not_found(name)),
    }
}

/// `GET /api/v1/domains`
pub async fn list_domains(ctx: &Ctx<'_>) -> ApiResponse {
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let domains = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::list_domains(db).unwrap_or_default())
        .await;
    let items = page.slice(&domains);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/domains/{domain}`
pub async fn get_domain(ctx: &Ctx<'_>, domain: &str) -> ApiResponse {
    let key = domain.to_string();
    let found = ctx
        .state
        .app
        .with_db_or(move |db| pangolin_core::db::get_domain(db, &key).unwrap_or(None))
        .await;
    match found {
        Some(d) => ApiResponse::json_of(200, &d),
        None => ApiResponse::error(&ApiError::not_found(
            "domain_not_found",
            format!("no domain named '{domain}'"),
        )),
    }
}

/// `GET /api/v1/dns_providers`
pub async fn list_dns_providers(ctx: &Ctx<'_>) -> ApiResponse {
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let providers = ctx
        .state
        .app
        .with_db_or(|db| {
            pangolin_core::db::list_dns_providers(db)
                .unwrap_or_default()
                .iter()
                .map(DnsProviderView::of)
                .collect::<Vec<_>>()
        })
        .await;
    let items = page.slice(&providers);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/dns_providers/{name}`
pub async fn get_dns_provider(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let key = name.to_string();
    let found = ctx
        .state
        .app
        .with_db_or(move |db| pangolin_core::db::get_dns_provider(db, &key).unwrap_or(None))
        .await;
    match found {
        Some(p) => ApiResponse::json_of(200, &DnsProviderView::of(&p)),
        None => ApiResponse::error(&ApiError::not_found(
            "dns_provider_not_found",
            format!("no DNS provider named '{name}'"),
        )),
    }
}

/// `GET /api/v1/tun`
pub async fn list_tuns(ctx: &Ctx<'_>) -> ApiResponse {
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let tuns = ctx
        .state
        .app
        .with_db_or(|db| {
            pangolin_core::db::list_tuns(db)
                .unwrap_or_default()
                .iter()
                .map(TunView::of)
                .collect::<Vec<_>>()
        })
        .await;
    let items = page.slice(&tuns);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/tun/{name}`
pub async fn get_tun(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let key = name.to_string();
    let found = ctx
        .state
        .app
        .with_db_or(move |db| pangolin_core::db::get_tun(db, &key).unwrap_or(None))
        .await;
    match found {
        Some(t) => ApiResponse::json_of(200, &TunView::of(&t)),
        None => ApiResponse::error(&ApiError::not_found(
            "tun_not_found",
            format!("no tunnel named '{name}'"),
        )),
    }
}

/// `GET /api/v1/ip_rules`
pub async fn list_ip_rules(ctx: &Ctx<'_>) -> ApiResponse {
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let rules = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::list_ip_rules(db).unwrap_or_default())
        .await;
    let items = page.slice(&rules);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/site_rate_limits`
pub async fn list_site_rate_limits(ctx: &Ctx<'_>) -> ApiResponse {
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let limits = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::list_site_rate_limits(db).unwrap_or_default())
        .await;
    let mut views = limits
        .into_iter()
        .map(|(site_name, policy)| SiteRateLimitView { site_name, policy })
        .collect::<Vec<_>>();
    // The map has no order of its own — a page needs one
    // (see the_per_site_lists_are_ordered_by_site_name).
    views.sort_by(|a, b| a.site_name.cmp(&b.site_name));
    let items = page.slice(&views);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/site_rate_limits/{site}`
pub async fn get_site_rate_limit(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let key = site.to_string();
    let policy = ctx
        .state
        .app
        .with_db_or(move |db| pangolin_core::db::get_site_rate_limit(db, &key).unwrap_or(None))
        .await;
    match policy {
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
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let bots = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::list_site_rate_limit_bots(db).unwrap_or_default())
        .await;
    let mut views = bots
        .into_iter()
        .map(|(site_name, bots)| SiteRateLimitBotsView {
            site_name,
            // The set is ordered, so the bots of a site keep their
            // `bot_name` order.
            bots: bots.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    // The map has no order of its own — a page needs one
    // (see the_per_site_lists_are_ordered_by_site_name).
    views.sort_by(|a, b| a.site_name.cmp(&b.site_name));
    let items = page.slice(&views);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/site_rate_limit_bots/{site}`
pub async fn get_site_rate_limit_bots(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let key = site.to_string();
    let bots = ctx
        .state
        .app
        .with_db_or(move |db| {
            pangolin_core::db::get_site_rate_limit_bots(db, &key).unwrap_or_default()
        })
        .await;
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
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let providers = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::list_site_providers(db).unwrap_or_default())
        .await;
    let mut views = providers
        .into_iter()
        .map(|(site_name, over)| SiteProviderView::of(&site_name, &over))
        .collect::<Vec<_>>();
    // The map has no order of its own — a page needs one
    // (see the_per_site_lists_are_ordered_by_site_name).
    views.sort_by(|a, b| a.site_name.cmp(&b.site_name));
    let items = page.slice(&views);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/site_providers/{site}`
pub async fn get_site_provider(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let key = site.to_string();
    let over = ctx
        .state
        .app
        .with_db_or(move |db| pangolin_core::db::get_site_provider(db, &key).unwrap_or(None))
        .await;
    match over {
        Some(over) => ApiResponse::json_of(200, &SiteProviderView::of(site, &over)),
        None => ApiResponse::error(&ApiError::not_found(
            "site_provider_not_found",
            format!("no provider override for '{site}'"),
        )),
    }
}

/// `GET /api/v1/site_origin_allow`
pub async fn list_site_origin_allow(ctx: &Ctx<'_>) -> ApiResponse {
    let page = match Page::from_ctx(ctx) {
        Ok(page) => page,
        Err(err) => return ApiResponse::error(&err),
    };
    let ranges = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::list_site_origin_allow(db).unwrap_or_default())
        .await;
    let mut views = ranges
        .into_iter()
        .map(|(site_name, cidrs)| SiteOriginAllowView { site_name, cidrs })
        .collect::<Vec<_>>();
    // The map has no order of its own — a page needs one
    // (see the_per_site_lists_are_ordered_by_site_name).
    views.sort_by(|a, b| a.site_name.cmp(&b.site_name));
    let items = page.slice(&views);
    ApiResponse::json_of(200, &items)
}

/// `GET /api/v1/site_origin_allow/{site}`
pub async fn get_site_origin_allow(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let ranges = ctx
        .state
        .app
        .with_db_or(|db| pangolin_core::db::list_site_origin_allow(db).unwrap_or_default())
        .await;
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
    let cfg = ctx.state.app.system_config.read().clone();
    ApiResponse::json_of(200, &*cfg)
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
    let view = BadbotsSettingsView::from(&settings);
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use pangolin_core::provider::SiteProviderOverride;
    use pangolin_core::ratelimit::{IpRule, RateLimitPolicy, RuleKind};
    use pangolin_core::types::{DnsProvider, DnsProviderKind, Domain, HostMode, Site, Tun};
    use pangolin_core::{App, CertManager};

    use crate::protocol::signature::dashboard_key;
    use crate::server::{ApiRequest, ApiState};

    /// The nine list endpoints that take the paging parameters.
    const LIST_ENDPOINTS: &[&str] = &[
        "/api/v1/sites",
        "/api/v1/domains",
        "/api/v1/dns_providers",
        "/api/v1/tun",
        "/api/v1/ip_rules",
        "/api/v1/site_rate_limits",
        "/api/v1/site_rate_limit_bots",
        "/api/v1/site_providers",
        "/api/v1/site_origin_allow",
    ];

    /// A gateway behind the router, on a temporary database.
    struct TestServer {
        app: Arc<App>,
        state: ApiState,
        _dir: tempfile::TempDir,
    }

    impl TestServer {
        /// Call the router and parse the answer as JSON.
        async fn call(&self, method: &str, path: &str) -> (u16, serde_json::Value) {
            let request = ApiRequest::new(method, path, Vec::new());
            let response = crate::server::route(&self.state, request).await;
            let json = serde_json::from_slice(&response.body).unwrap_or(serde_json::Value::Null);
            (response.status, json)
        }

        /// Call the router with a body and parse the answer as JSON.
        async fn call_with(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
        ) -> (u16, serde_json::Value) {
            let request = ApiRequest::new(method, path, body.to_vec());
            let response = crate::server::route(&self.state, request).await;
            let json = serde_json::from_slice(&response.body).unwrap_or(serde_json::Value::Null);
            (response.status, json)
        }
    }

    /// Start a gateway behind the router.
    async fn start_api() -> TestServer {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("pangolin.db");
        // The log directories point into the tempdir — the default ones
        // are relative to the working directory, and a test must not
        // write into the repository.
        let mut config = pangolin_core::Config::default();
        config.log.bot.dir = dir.path().join("logs/bots");
        config.log.bot.cache_dir = dir.path().join("logs/bots/cache");
        config.log.referer.dir = dir.path().join("logs/referers");
        let app = Arc::new(
            App::new(
                db_path.to_str().expect("utf8"),
                config,
                CertManager::default(),
            )
            .expect("App::new"),
        );
        let state = ApiState::new(app.clone(), dashboard_key());
        TestServer {
            app,
            state,
            _dir: dir,
        }
    }

    /// Seed one of every resource, so every list has an entry.
    ///
    /// `n` is the last octet of the address the rule and the allow
    /// range carry — the two are addresses, so they have to differ.
    ///
    /// The writes go through `App::with_db_res` — the blocking pool,
    /// like every other query of the gateway. A test must not take
    /// the database lock on an async worker either.
    async fn seed_all(app: &App, name: &str, n: u8) {
        let name = name.to_string();
        app.with_db_res(move |db| {
            let site_name = name.clone();
            let now = chrono::Utc::now();

            pangolin_core::db::upsert_site(
                db,
                &Site {
                    name: site_name.clone(),
                    backend: "http://127.0.0.1:8080".into(),
                    enabled: true,
                    created_at: now,
                    updated_at: now,
                    host_mode: HostMode::Passthrough,
                    host_custom: None,
                    domain_count: 0,
                },
            )?;

            pangolin_core::db::upsert_domain(
                db,
                &Domain {
                    domain: format!("{site_name}.test"),
                    site_name: site_name.clone(),
                    enabled: true,
                    auto_issue: false,
                    dns_provider: None,
                    challenge_kind: None,
                    created_at: now,
                },
            )?;

            pangolin_core::db::upsert_dns_provider(
                db,
                &DnsProvider {
                    name: site_name.clone(),
                    kind: DnsProviderKind::Cloudflare,
                    enabled: true,
                    config: "{}".into(),
                    created_at: now,
                    updated_at: now,
                },
            )?;

            pangolin_core::db::upsert_tun(
                db,
                &Tun {
                    name: site_name.clone(),
                    token: None,
                    token_hash: None,
                    enabled: true,
                    online: false,
                    registered_at: None,
                    last_seen_at: None,
                    expires_at: None,
                },
            )?;

            let cidr = format!("10.0.0.{n}");
            pangolin_core::db::upsert_ip_rule(
                db,
                &IpRule {
                    id: format!("global|blacklist|{cidr}"),
                    scope: "global".into(),
                    kind: RuleKind::Blacklist,
                    cidr: cidr.clone(),
                    note: None,
                    created_at: now,
                },
            )?;

            pangolin_core::db::upsert_site_rate_limit(db, &site_name, &RateLimitPolicy::default())?;

            pangolin_core::db::set_site_rate_limit_bots(
                db,
                &site_name,
                &BTreeSet::from([format!("bot-{n}")]),
            )?;

            pangolin_core::db::upsert_site_provider(
                db,
                &site_name,
                &SiteProviderOverride::default(),
            )?;

            pangolin_core::db::replace_site_origin_allow(db, &site_name, &[cidr])?;
            Ok(())
        })
        .await
        .expect("the seed");
    }

    /// The entries of an answer, or a panic that names the answer.
    fn entries<'a>(path: &str, json: &'a serde_json::Value) -> &'a [serde_json::Value] {
        json.as_array()
            .unwrap_or_else(|| panic!("{path} -> {json}"))
    }

    /// Every list endpoint answers a page of one.
    ///
    /// The regression this pins: a list endpoint that forgot the
    /// paging would answer the whole list whatever the `limit` says.
    #[tokio::test]
    async fn every_list_endpoint_answers_a_page() {
        let server = start_api().await;
        seed_all(&server.app, "one", 1).await;
        seed_all(&server.app, "two", 2).await;

        for path in LIST_ENDPOINTS {
            let (status, json) = server.call("GET", &format!("{path}?limit=1")).await;
            assert_eq!(status, 200, "{path} -> {json}");
            assert_eq!(entries(path, &json).len(), 1, "{path} -> {json}");

            let (status, json) = server.call("GET", &format!("{path}?offset=1")).await;
            assert_eq!(status, 200, "{path} -> {json}");
            assert_eq!(entries(path, &json).len(), 1, "{path} -> {json}");
        }
    }

    /// A list is read in the same order with and without the paging
    /// parameters, and no entry is lost or repeated between the pages.
    #[tokio::test]
    async fn a_long_list_is_paged_without_losing_an_entry() {
        let server = start_api().await;
        server
            .app
            .with_db_res(|db| {
                // One transaction — 1200 single writes would be 1200
                // commits.
                let tx = db.unchecked_transaction()?;
                let now = chrono::Utc::now();
                for n in 0..1200 {
                    pangolin_core::db::upsert_site(
                        &tx,
                        &Site {
                            name: format!("site-{n:04}"),
                            backend: "http://127.0.0.1:8080".into(),
                            enabled: true,
                            created_at: now,
                            updated_at: now,
                            host_mode: HostMode::Passthrough,
                            host_custom: None,
                            domain_count: 0,
                        },
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .expect("the sites");

        let (status, all) = server.call("GET", "/api/v1/sites").await;
        assert_eq!(status, 200, "{all}");
        let expected = entries("/api/v1/sites", &all)
            .iter()
            .map(|e| e["name"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(expected.len(), 1200);

        let mut paged = Vec::new();
        for offset in (0..1200).step_by(500) {
            let path = format!("/api/v1/sites?limit=500&offset={offset}");
            let (status, page) = server.call("GET", &path).await;
            assert_eq!(status, 200, "{path} -> {page}");
            let page_entries = entries(&path, &page);
            assert!(page_entries.len() <= 500, "{path} -> {page}");
            paged.extend(
                page_entries
                    .iter()
                    .map(|e| e["name"].as_str().unwrap_or_default().to_string()),
            );
        }

        let mut unique = paged.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            1200,
            "an entry was repeated between the pages"
        );
        assert_eq!(
            paged, expected,
            "the pages have to follow the order of the whole list"
        );
    }

    /// The two parameters are independent.
    #[tokio::test]
    async fn the_limit_and_the_offset_are_independent() {
        fn names(json: &serde_json::Value) -> Vec<String> {
            json.as_array()
                .expect("the list")
                .iter()
                .map(|e| e["name"].as_str().unwrap_or_default().to_string())
                .collect()
        }

        let server = start_api().await;
        for n in 1..=5 {
            seed_all(&server.app, &format!("site-{n}"), n).await;
        }

        let (_, json) = server.call("GET", "/api/v1/sites?limit=2").await;
        assert_eq!(names(&json), vec!["site-1", "site-2"], "{json}");

        let (_, json) = server.call("GET", "/api/v1/sites?offset=3").await;
        assert_eq!(names(&json), vec!["site-4", "site-5"], "{json}");

        let (_, json) = server.call("GET", "/api/v1/sites?limit=2&offset=3").await;
        assert_eq!(names(&json), vec!["site-4", "site-5"], "{json}");
    }

    /// A paged answer is still a bare JSON array.
    ///
    /// The regression this pins: the paging must not wrap the list in
    /// an envelope or add a total to it — the shape of the answer is
    /// the shape it was.
    #[tokio::test]
    async fn a_paged_answer_is_a_bare_json_array() {
        let server = start_api().await;
        seed_all(&server.app, "one", 1).await;
        seed_all(&server.app, "two", 2).await;

        let request = ApiRequest::new("GET", "/api/v1/sites?limit=1", Vec::new());
        let response = crate::server::route(&server.state, request).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json");
        let json: serde_json::Value =
            serde_json::from_slice(&response.body).expect("the body is json");
        assert!(json.is_array(), "{json}");
        assert_eq!(json.as_array().map(|a| a.len()), Some(1), "{json}");
    }

    /// A parameter that is not an integer is refused.
    #[tokio::test]
    async fn a_bad_paging_parameter_is_refused() {
        let server = start_api().await;
        for (query, code, field) in [
            ("?limit=abc", "limit_invalid", "limit"),
            ("?limit=1.5", "limit_invalid", "limit"),
            ("?limit=0", "limit_invalid", "limit"),
            ("?limit=", "limit_invalid", "limit"),
            ("?offset=-1", "offset_invalid", "offset"),
            ("?offset=abc", "offset_invalid", "offset"),
        ] {
            let path = format!("/api/v1/sites{query}");
            let (status, json) = server.call("GET", &path).await;
            assert_eq!(status, 422, "{path} -> {json}");
            assert_eq!(json["error"]["code"], code, "{path} -> {json}");
            assert_eq!(json["error"]["field"], field, "{path} -> {json}");
        }
    }

    /// The query string spells the parameters the way it wants to.
    ///
    /// The regression this pins: the paging reads its parameters the
    /// way the rest of the API reads its query string — a percent-
    /// encoded name is the same name, and a repeated parameter is the
    /// first one.
    #[tokio::test]
    async fn the_paging_parameters_are_read_the_way_the_query_string_spells_them() {
        let server = start_api().await;
        seed_all(&server.app, "one", 1).await;
        seed_all(&server.app, "two", 2).await;

        let (status, json) = server.call("GET", "/api/v1/sites?%6Cimit=1").await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(entries("/api/v1/sites", &json).len(), 1, "{json}");

        let (status, json) = server.call("GET", "/api/v1/sites?limit=1&limit=2").await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(entries("/api/v1/sites", &json).len(), 1, "{json}");
    }

    /// An offset past the end of the list is an empty list — reading to
    /// the end is a normal path, not an error.
    #[tokio::test]
    async fn an_offset_past_the_end_is_an_empty_list() {
        let server = start_api().await;
        seed_all(&server.app, "one", 1).await;
        let (status, json) = server.call("GET", "/api/v1/sites?offset=100000").await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json, serde_json::json!([]));
    }

    /// The per-site lists are ordered by their site name.
    ///
    /// The regression this pins: the lists come out of a `HashMap`, so
    /// without an order of their own a page could repeat an entry or
    /// skip one.
    #[tokio::test]
    async fn the_per_site_lists_are_ordered_by_site_name() {
        let server = start_api().await;
        seed_all(&server.app, "b-site", 2).await;
        seed_all(&server.app, "a-site", 1).await;

        for path in [
            "/api/v1/site_rate_limits",
            "/api/v1/site_rate_limit_bots",
            "/api/v1/site_providers",
            "/api/v1/site_origin_allow",
        ] {
            let (status, json) = server.call("GET", path).await;
            assert_eq!(status, 200, "{path} -> {json}");
            let names = entries(path, &json)
                .iter()
                .map(|e| e["site_name"].as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                vec!["a-site".to_string(), "b-site".to_string()],
                "{path} -> {json}"
            );
        }
    }

    /// A single resource ignores the paging parameters.
    #[tokio::test]
    async fn a_single_resource_ignores_the_paging_parameters() {
        let server = start_api().await;
        seed_all(&server.app, "one", 1).await;
        let (status, plain) = server.call("GET", "/api/v1/sites/one").await;
        assert_eq!(status, 200, "{plain}");
        let (status, paged) = server.call("GET", "/api/v1/sites/one?limit=2").await;
        assert_eq!(status, 200, "{paged}");
        assert_eq!(plain, paged, "a single resource is not paged");
    }

    /// A write ignores the paging parameters too.
    #[tokio::test]
    async fn a_write_ignores_the_paging_parameters() {
        let server = start_api().await;
        let body = serde_json::json!({
            "name": "written",
            "backend": "http://127.0.0.1:8080",
        })
        .to_string();
        let (status, json) = server
            .call_with("POST", "/api/v1/sites?limit=2", body.as_bytes())
            .await;
        assert_eq!(status, 201, "{json}");
    }

    /// The paging is not a new capability — the published view of the
    /// node does not change.
    #[tokio::test]
    async fn the_paging_is_not_a_new_capability() {
        let server = start_api().await;
        let (status, json) = server.call("GET", "/api/v1/instance").await;
        assert_eq!(status, 200, "{json}");
        let capabilities = json["capabilities"]
            .as_array()
            .unwrap_or_else(|| panic!("the capabilities -> {json}"));
        assert!(
            !capabilities.iter().any(|c| c == "paging"),
            "{capabilities:?}"
        );
    }
}
