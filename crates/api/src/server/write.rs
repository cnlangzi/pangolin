//! The write handlers.
//!
//! Every write validates through the **admin UI's own parsers**, so a
//! body the HTML form would reject is rejected here too — with the
//! same message (issue #185, R11). The write semantics differ per
//! resource and are documented in [`docs/design/api.md`](../../../docs/design/api.md):
//!
//! | resource | semantics |
//! | --- | --- |
//! | `site_rate_limit_bots`, `site_origin_allow` | **replace** |
//! | `system_config`, `badbots_config` | **merge** |
//! | everything else | **full replace** |

use std::collections::BTreeSet;

use pangolin_core::types::{Domain, Site};

use super::read;
use super::{ApiResponse, Ctx};
use crate::dto::write::{
    BadbotsConfigDto, DnsProviderDto, DomainDto, IpRuleDto, RateLimitDto, SiteDto, SiteProviderDto,
    SystemConfigDto, TunDto, encode_form,
};
use crate::protocol::error::ApiError;

/// The error code of a rejected site form.
fn site_form_code(err: &admin::routes::sites::helpers::SiteFormError) -> String {
    use admin::routes::sites::helpers::SiteFormSection;
    match err.section {
        SiteFormSection::Provider => "provider_invalid".to_string(),
        SiteFormSection::RateLimit => "rate_limit_invalid".to_string(),
        SiteFormSection::Guard => "origin_guard_invalid".to_string(),
        SiteFormSection::Field => match err.field {
            Some(field) => format!("{field}_invalid"),
            None => "site_invalid".to_string(),
        },
    }
}

// ── Sites ───────────────────────────────────────────────────────────────

/// `POST /api/v1/sites`
pub async fn create_site(ctx: &Ctx<'_>) -> ApiResponse {
    let dto: SiteDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let name = dto.name.clone().unwrap_or_default();
    let global_provider = ctx.state.app.system_config.read().await.provider;
    let params = dto.to_params();
    let parsed =
        match admin::routes::sites::helpers::parse_site_form(&params, &name, None, global_provider)
        {
            Ok(p) => p,
            Err(e) => {
                return ApiResponse::error(&ApiError::unprocessable(
                    site_form_code(&e),
                    e.message.clone(),
                    e.field,
                ));
            }
        };

    {
        let db = ctx.state.app.db.lock().await;
        if pangolin_core::db::get_site(&db, &parsed.name)
            .unwrap_or(None)
            .is_some()
        {
            return ApiResponse::error(&read::site_exists(&parsed.name));
        }
    }

    let site = Site {
        name: parsed.name.clone(),
        backend: parsed.backend.clone(),
        enabled: dto.enabled.unwrap_or(true),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        host_mode: parsed.host_mode,
        host_custom: parsed.host_custom.clone(),
        domain_count: 0,
    };

    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_site(&db, &site) {
            return ApiResponse::error(&read::database_error(e));
        }
        if let Some(policy) = &parsed.rate_limit
            && let Err(e) = pangolin_core::db::upsert_site_rate_limit(&db, &site.name, policy)
        {
            return ApiResponse::error(&read::database_error(e));
        }
        if parsed.provider.present
            && let Err(e) = write_site_provider(&db, &site.name, &parsed.provider)
        {
            return ApiResponse::error(&read::database_error(e));
        }
        if let Some(bots) = &parsed.rate_limit_bots
            && let Err(e) = pangolin_core::db::set_site_rate_limit_bots(&db, &site.name, bots)
        {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(201, &site)
}

/// `PUT /api/v1/sites/{name}`
pub async fn update_site(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let dto: SiteDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let stored_policy = {
        let db = ctx.state.app.db.lock().await;
        pangolin_core::db::get_site_rate_limit(&db, name)
            .ok()
            .flatten()
    };
    let global_provider = ctx.state.app.system_config.read().await.provider;
    let params = dto.to_params();
    let parsed = match admin::routes::sites::helpers::parse_site_form(
        &params,
        name,
        stored_policy.as_ref(),
        global_provider,
    ) {
        Ok(p) => p,
        Err(e) => {
            return ApiResponse::error(&ApiError::unprocessable(
                site_form_code(&e),
                e.message.clone(),
                e.field,
            ));
        }
    };

    let existing = {
        let db = ctx.state.app.db.lock().await;
        pangolin_core::db::get_site(&db, name).unwrap_or(None)
    };
    let Some(existing) = existing else {
        return ApiResponse::error(&read::site_not_found(name));
    };

    let site = Site {
        name: parsed.name.clone(),
        backend: parsed.backend.clone(),
        enabled: dto.enabled.unwrap_or(existing.enabled),
        created_at: existing.created_at,
        updated_at: chrono::Utc::now(),
        host_mode: parsed.host_mode,
        host_custom: parsed.host_custom.clone(),
        domain_count: existing.domain_count,
    };

    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_site(&db, &site) {
            return ApiResponse::error(&read::database_error(e));
        }
        if let Some(policy) = &parsed.rate_limit
            && let Err(e) = pangolin_core::db::upsert_site_rate_limit(&db, name, policy)
        {
            return ApiResponse::error(&read::database_error(e));
        }
        if parsed.provider.present
            && let Err(e) = write_site_provider(&db, name, &parsed.provider)
        {
            return ApiResponse::error(&read::database_error(e));
        }
        if let Some(bots) = &parsed.rate_limit_bots
            && let Err(e) = pangolin_core::db::set_site_rate_limit_bots(&db, name, bots)
        {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(200, &site)
}

/// `DELETE /api/v1/sites/{name}` — idempotent.
pub async fn delete_site(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    {
        let db = ctx.state.app.db.lock().await;
        let _ = pangolin_core::db::delete_site(&db, name);
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json(200, &serde_json::json!({ "name": name, "deleted": true }))
}

/// Write a site's provider override (and its manual ranges).
fn write_site_provider(
    db: &rusqlite::Connection,
    site_name: &str,
    parsed: &admin::routes::provider::ParsedProvider,
) -> rusqlite::Result<()> {
    match &parsed.over {
        Some(over) => pangolin_core::db::upsert_site_provider(db, site_name, over)?,
        None => {
            pangolin_core::db::delete_site_provider(db, site_name)?;
        }
    }
    if parsed.origin_allow_present {
        pangolin_core::db::replace_site_origin_allow(db, site_name, &parsed.manual_ranges)?;
    }
    Ok(())
}

// ── Domains ─────────────────────────────────────────────────────────────

/// `POST /api/v1/domains`
pub async fn create_domain(ctx: &Ctx<'_>) -> ApiResponse {
    let dto: DomainDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let domain = dto.domain.clone().unwrap_or_default();
    let params = dto.to_params();
    let parsed = {
        let db = ctx.state.app.db.lock().await;
        admin::routes::domains::mutate::parse_domain_form(&params, &domain, None, &|name| {
            pangolin_core::db::get_dns_provider(&db, name)
                .unwrap_or(None)
                .is_some()
        })
    };
    let parsed = match parsed {
        Ok(p) => p,
        Err(msg) => {
            return ApiResponse::error(&ApiError::unprocessable("domain_invalid", msg, None));
        }
    };

    {
        let db = ctx.state.app.db.lock().await;
        if pangolin_core::db::get_domain(&db, &domain)
            .unwrap_or(None)
            .is_some()
        {
            return ApiResponse::error(&read::domain_exists(&domain));
        }
    }

    let d = Domain {
        domain: domain.clone(),
        site_name: parsed.site_name,
        enabled: dto.enabled.unwrap_or(parsed.enabled),
        auto_issue: dto.auto_issue.unwrap_or(parsed.auto_issue),
        dns_provider: parsed.dns_provider,
        challenge_kind: parsed.challenge_kind,
        created_at: chrono::Utc::now(),
    };

    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_domain(&db, &d) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(201, &d)
}

/// `PUT /api/v1/domains/{domain}`
pub async fn update_domain(ctx: &Ctx<'_>, domain: &str) -> ApiResponse {
    let dto: DomainDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let existing = {
        let db = ctx.state.app.db.lock().await;
        pangolin_core::db::get_domain(&db, domain).unwrap_or(None)
    };
    let Some(existing) = existing else {
        return ApiResponse::error(&ApiError::not_found(
            "domain_not_found",
            format!("no domain named '{domain}'"),
        ));
    };
    let params = dto.to_params();
    let parsed = {
        let db = ctx.state.app.db.lock().await;
        admin::routes::domains::mutate::parse_domain_form(
            &params,
            domain,
            Some(&existing),
            &|name| {
                pangolin_core::db::get_dns_provider(&db, name)
                    .unwrap_or(None)
                    .is_some()
            },
        )
    };
    let parsed = match parsed {
        Ok(p) => p,
        Err(msg) => {
            return ApiResponse::error(&ApiError::unprocessable("domain_invalid", msg, None));
        }
    };

    let updated = Domain {
        domain: existing.domain.clone(),
        site_name: parsed.site_name,
        enabled: dto.enabled.unwrap_or(parsed.enabled),
        auto_issue: dto.auto_issue.unwrap_or(parsed.auto_issue),
        dns_provider: parsed.dns_provider,
        challenge_kind: parsed.challenge_kind,
        created_at: existing.created_at,
    };
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_domain(&db, &updated) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(200, &updated)
}

/// `DELETE /api/v1/domains/{domain}` — idempotent.
pub async fn delete_domain(ctx: &Ctx<'_>, domain: &str) -> ApiResponse {
    {
        let db = ctx.state.app.db.lock().await;
        let _ = pangolin_core::db::delete_domain(&db, domain);
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json(
        200,
        &serde_json::json!({ "domain": domain, "deleted": true }),
    )
}

// ── DNS providers ───────────────────────────────────────────────────────

/// `POST /api/v1/dns_providers`
pub async fn create_dns_provider(ctx: &Ctx<'_>) -> ApiResponse {
    let dto: DnsProviderDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let name = dto.name.clone().unwrap_or_default();
    let params = dto.to_params();
    let parsed = match admin::routes::dns::mutate::parse_dns_provider_form(&params, &name, None) {
        Ok(p) => p,
        Err(msg) => {
            return ApiResponse::error(&ApiError::unprocessable("dns_provider_invalid", msg, None));
        }
    };
    {
        let db = ctx.state.app.db.lock().await;
        if pangolin_core::db::get_dns_provider(&db, &parsed.name)
            .unwrap_or(None)
            .is_some()
        {
            return ApiResponse::error(&read::dns_provider_exists(&parsed.name));
        }
    }
    let now = chrono::Utc::now();
    let provider = pangolin_core::types::DnsProvider {
        name: parsed.name,
        kind: parsed.kind,
        enabled: dto.enabled.unwrap_or(parsed.enabled),
        config: parsed.config,
        created_at: now,
        updated_at: now,
    };
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_dns_provider(&db, &provider) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(201, &crate::dto::views::DnsProviderView::of(&provider))
}

/// `PUT /api/v1/dns_providers/{name}`
pub async fn update_dns_provider(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let dto: DnsProviderDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let existing = {
        let db = ctx.state.app.db.lock().await;
        pangolin_core::db::get_dns_provider(&db, name).unwrap_or(None)
    };
    let Some(existing) = existing else {
        return ApiResponse::error(&ApiError::not_found(
            "dns_provider_not_found",
            format!("no DNS provider named '{name}'"),
        ));
    };
    let params = dto.to_params();
    let parsed =
        match admin::routes::dns::mutate::parse_dns_provider_form(&params, name, Some(&existing)) {
            Ok(p) => p,
            Err(msg) => {
                return ApiResponse::error(&ApiError::unprocessable(
                    "dns_provider_invalid",
                    msg,
                    None,
                ));
            }
        };
    let provider = pangolin_core::types::DnsProvider {
        name: parsed.name,
        kind: parsed.kind,
        enabled: dto.enabled.unwrap_or(parsed.enabled),
        config: parsed.config,
        created_at: existing.created_at,
        updated_at: chrono::Utc::now(),
    };
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_dns_provider(&db, &provider) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(200, &crate::dto::views::DnsProviderView::of(&provider))
}

/// `DELETE /api/v1/dns_providers/{name}` — idempotent.
pub async fn delete_dns_provider(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    {
        let db = ctx.state.app.db.lock().await;
        let _ = pangolin_core::db::delete_dns_provider(&db, name);
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json(200, &serde_json::json!({ "name": name, "deleted": true }))
}

// ── Tunnel nodes ────────────────────────────────────────────────────────

/// `POST /api/v1/tun`
///
/// The token is returned **once**, in this response — it is never
/// readable again.
pub async fn create_tun(ctx: &Ctx<'_>) -> ApiResponse {
    let dto: TunDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let name = dto.name.clone().unwrap_or_default();
    let params = match dto.to_params() {
        Ok(p) => p,
        Err(e) => return ApiResponse::error(&e),
    };
    let parsed = match admin::routes::tun::mutate::parse_tun_form(&params, &name, None) {
        Ok(p) => p,
        Err(msg) => {
            return ApiResponse::error(&ApiError::unprocessable("tun_invalid", msg, None));
        }
    };
    {
        let db = ctx.state.app.db.lock().await;
        if pangolin_core::db::get_tun(&db, &name)
            .unwrap_or(None)
            .is_some()
        {
            return ApiResponse::error(&read::tun_exists(&name));
        }
    }
    // A submission without a token gets a fresh one.
    let token = match parsed.token {
        Some(t) => t,
        None => match admin::routes::tun::helpers::generate_token() {
            Ok(t) => t,
            Err(e) => {
                return ApiResponse::error(&ApiError::internal(format!(
                    "the token could not be generated: {e}"
                )));
            }
        },
    };
    let tun = pangolin_core::types::Tun {
        name: parsed.name,
        token: Some(token.clone()),
        token_hash: None,
        enabled: parsed.enabled,
        online: false,
        registered_at: None,
        last_seen_at: None,
        expires_at: parsed.expires_at,
    };
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_tun(&db, &tun) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    let view = crate::dto::views::TunView::of(&tun);
    ApiResponse::json(
        201,
        &serde_json::json!({
            "tun": view,
            // The token is returned exactly once — here.
            "token": token,
        }),
    )
}

/// `PUT /api/v1/tun/{name}`
///
/// The token is returned **once**, in this response — and only when
/// the body rotated it.
pub async fn update_tun(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    let dto: TunDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let existing = {
        let db = ctx.state.app.db.lock().await;
        pangolin_core::db::get_tun(&db, name).unwrap_or(None)
    };
    let Some(mut tun) = existing else {
        return ApiResponse::error(&ApiError::not_found(
            "tun_not_found",
            format!("no tunnel named '{name}'"),
        ));
    };
    let params = match dto.to_params() {
        Ok(p) => p,
        Err(e) => return ApiResponse::error(&e),
    };
    let parsed = match admin::routes::tun::mutate::parse_tun_form(&params, name, Some(&tun)) {
        Ok(p) => p,
        Err(msg) => {
            return ApiResponse::error(&ApiError::unprocessable("tun_invalid", msg, None));
        }
    };
    let rotated = parsed.token.clone();
    if let Some(new_token) = rotated.clone() {
        tun.token = Some(new_token);
    }
    tun.expires_at = parsed.expires_at;
    tun.enabled = parsed.enabled;
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_tun(&db, &tun) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    let view = crate::dto::views::TunView::of(&tun);
    let mut body = serde_json::json!({ "tun": view });
    if let Some(token) = rotated {
        body["token"] = serde_json::Value::String(token);
    }
    ApiResponse::json(200, &body)
}

/// `DELETE /api/v1/tun/{name}` — idempotent.
pub async fn delete_tun(ctx: &Ctx<'_>, name: &str) -> ApiResponse {
    {
        let db = ctx.state.app.db.lock().await;
        let _ = pangolin_core::db::delete_tun(&db, name);
    }
    ApiResponse::json(200, &serde_json::json!({ "name": name, "deleted": true }))
}

// ── IP rules ────────────────────────────────────────────────────────────

/// `POST /api/v1/ip_rules`
pub async fn create_ip_rule(ctx: &Ctx<'_>) -> ApiResponse {
    let dto: IpRuleDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let params = dto.to_params();
    let rule = {
        let db = ctx.state.app.db.lock().await;
        let sites = pangolin_core::db::list_sites(&db)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>();
        admin::routes::ratelimit::parse_ip_rule_form(&params, &|scope| {
            sites.iter().any(|s| s == scope)
        })
    };
    let rule = match rule {
        Ok(r) => r,
        Err(e) => {
            let code = match e {
                admin::routes::ratelimit::IpRuleFormError::InvalidKind => "ip_rule_kind_invalid",
                admin::routes::ratelimit::IpRuleFormError::InvalidCidr => "ip_rule_cidr_invalid",
                admin::routes::ratelimit::IpRuleFormError::UnknownScope => "ip_rule_scope_invalid",
            };
            return ApiResponse::error(&ApiError::unprocessable(code, e.message(), None));
        }
    };
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_ip_rule(&db, &rule) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(201, &rule)
}

/// `DELETE /api/v1/ip_rules/{id}` — idempotent.
///
/// The id is the rule's natural key, `<scope>|<kind>|<cidr>`.
pub async fn delete_ip_rule(ctx: &Ctx<'_>, id: &str) -> ApiResponse {
    {
        let db = ctx.state.app.db.lock().await;
        let _ = pangolin_core::db::delete_ip_rule(&db, id);
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json(200, &serde_json::json!({ "id": id, "deleted": true }))
}

// ── The per-site rate limits ────────────────────────────────────────────

/// `PUT /api/v1/site_rate_limits/{site}` — full replace.
pub async fn put_site_rate_limit(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let dto: RateLimitDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let params = dto.to_params();
    let parsed = admin::routes::ratelimit::parse_policy(&params);
    if let Some(msg) = &parsed.error {
        return ApiResponse::error(&ApiError::unprocessable(
            "rate_limit_invalid",
            msg.clone(),
            Some("rate_limit"),
        ));
    }
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::upsert_site_rate_limit(&db, site, &parsed.policy) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(
        200,
        &crate::dto::views::SiteRateLimitView {
            site_name: site.to_string(),
            policy: parsed.policy,
        },
    )
}

/// `DELETE /api/v1/site_rate_limits/{site}` — idempotent.
pub async fn delete_site_rate_limit(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    {
        let db = ctx.state.app.db.lock().await;
        let _ = pangolin_core::db::delete_site_rate_limit(&db, site);
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json(
        200,
        &serde_json::json!({ "site_name": site, "deleted": true }),
    )
}

/// `PUT /api/v1/site_rate_limit_bots/{site}` — replace.
///
/// The list the body carries **is** the site's list: an entry the
/// body omits is removed.
pub async fn put_site_rate_limit_bots(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        bots: Vec<String>,
    }
    let body: Body = match ctx.json_body() {
        Ok(b) => b,
        Err(e) => return ApiResponse::error(&e),
    };
    let mut bots = BTreeSet::new();
    for id in &body.bots {
        if !pangolin_core::bot::is_safe_bot_id(id) {
            return ApiResponse::error(&ApiError::unprocessable(
                "bot_id_invalid",
                format!("'{id}' is not a valid bot id"),
                Some("bots"),
            ));
        }
        bots.insert(id.clone());
    }
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::set_site_rate_limit_bots(&db, site, &bots) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(
        200,
        &crate::dto::views::SiteRateLimitBotsView {
            site_name: site.to_string(),
            bots: bots.into_iter().collect(),
        },
    )
}

// ── The per-site provider override ──────────────────────────────────────

/// `PUT /api/v1/site_providers/{site}` — full replace.
pub async fn put_site_provider(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    let dto: SiteProviderDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let mut params = crate::dto::write::Params::new();
    dto.write_params(&mut params);
    let parsed = admin::routes::provider::parse_site_provider(&params);
    if let Some(msg) = &parsed.error {
        return ApiResponse::error(&ApiError::unprocessable(
            "provider_invalid",
            msg.clone(),
            Some("provider"),
        ));
    }
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = write_site_provider(&db, site, &parsed) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    let view = {
        let db = ctx.state.app.db.lock().await;
        pangolin_core::db::get_site_provider(&db, site)
            .unwrap_or(None)
            .map(|over| crate::dto::views::SiteProviderView::of(site, &over))
    };
    match view {
        Some(v) => ApiResponse::json_of(200, &v),
        None => ApiResponse::json(
            200,
            &serde_json::json!({ "site_name": site, "provider": null }),
        ),
    }
}

/// `DELETE /api/v1/site_providers/{site}` — idempotent.
pub async fn delete_site_provider(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    {
        let db = ctx.state.app.db.lock().await;
        let _ = pangolin_core::db::delete_site_provider(&db, site);
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json(
        200,
        &serde_json::json!({ "site_name": site, "deleted": true }),
    )
}

// ── The per-site manual ranges ──────────────────────────────────────────

/// `PUT /api/v1/site_origin_allow/{site}` — replace.
///
/// The list the body carries **is** the site's list: an entry the
/// body omits is removed.
pub async fn put_site_origin_allow(ctx: &Ctx<'_>, site: &str) -> ApiResponse {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        cidrs: Vec<String>,
    }
    let body: Body = match ctx.json_body() {
        Ok(b) => b,
        Err(e) => return ApiResponse::error(&e),
    };
    let mut cidrs = Vec::with_capacity(body.cidrs.len());
    for raw in &body.cidrs {
        match pangolin_core::cidr::normalize_cidr(raw) {
            Ok(c) => cidrs.push(c),
            Err(e) => {
                return ApiResponse::error(&ApiError::unprocessable(
                    "cidr_invalid",
                    format!("Invalid source range: {e}"),
                    Some("cidrs"),
                ));
            }
        }
    }
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::replace_site_origin_allow(&db, site, &cidrs) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(
        200,
        &crate::dto::views::SiteOriginAllowView {
            site_name: site.to_string(),
            cidrs,
        },
    )
}

// ── The singletons ──────────────────────────────────────────────────────

/// `PUT /api/v1/system_config` — merge.
pub async fn put_system_config(ctx: &Ctx<'_>) -> ApiResponse {
    let dto: SystemConfigDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let current = ctx.state.app.system_config.read().await.clone();
    let params = dto.to_params(&current);
    let body = encode_form(&params);
    let parsed = match admin::routes::system::parse_system_config_form_merged(
        &ctx.state.app,
        body.as_bytes(),
    )
    .await
    {
        Ok(c) => c,
        Err(msg) => {
            return ApiResponse::error(&ApiError::unprocessable(
                "system_config_invalid",
                msg,
                None,
            ));
        }
    };
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::update_system_config(&db, &parsed) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.reload_indexes().await;
    ApiResponse::json_of(200, &parsed)
}

/// `PUT /api/v1/badbots_config` — merge.
pub async fn put_badbots_config(ctx: &Ctx<'_>) -> ApiResponse {
    let dto: BadbotsConfigDto = match ctx.json_body() {
        Ok(d) => d,
        Err(e) => return ApiResponse::error(&e),
    };
    let current = match ctx.state.app.badbots_settings().await {
        Ok(c) => c,
        Err(e) => {
            return ApiResponse::error(&ApiError::internal(format!(
                "reading the reputation configuration failed: {e}"
            )));
        }
    };
    let params = dto.to_params();
    let body = encode_form(&params);
    let parsed = match admin::routes::badbots::parse_badbots_form(body.as_bytes(), &current) {
        Ok(c) => c,
        Err(msg) => {
            return ApiResponse::error(&ApiError::unprocessable(
                "badbots_config_invalid",
                msg,
                None,
            ));
        }
    };
    {
        let db = ctx.state.app.db.lock().await;
        if let Err(e) = pangolin_core::db::update_badbots_config(&db, &parsed) {
            return ApiResponse::error(&read::database_error(e));
        }
    }
    ctx.state.app.rebuild_badbots().await;
    let view = crate::dto::views::BadbotsSettingsView {
        enabled: parsed.enabled,
        evidence_policy: parsed.evidence_policy.as_str().to_string(),
        has_abuseipdb_api_key: parsed.abuseipdb_api_key.is_some(),
        has_crowdsec_api_key: parsed.crowdsec_api_key.is_some(),
        updated_at: parsed.updated_at,
    };
    ApiResponse::json_of(200, &view)
}

// ── The reload ──────────────────────────────────────────────────────────

/// `POST /api/v1/reload` — re-read every index from the database.
pub async fn reload(ctx: &Ctx<'_>) -> ApiResponse {
    ctx.state.app.reload_indexes().await;
    ApiResponse::json(
        200,
        &serde_json::json!({
            "status": "ok",
            "message": "Configuration reloaded successfully.",
        }),
    )
}
