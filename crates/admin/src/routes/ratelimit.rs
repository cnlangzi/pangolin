//! `/ratelimit` — per-site rate limits and IP rules (issue #117).
//!
//! The page is server-rendered from [`App::ratelimit_snapshot`] (the
//! live counters) plus the `ip_rules` table (the configuration). HTMX
//! polls `/api/ratelimit/kpis` and `/api/ratelimit/sites` (2s) and
//! `/api/ratelimit/rules` (5s).
//!
//! The counters are in-memory only — they are reset when the process
//! restarts. The configuration (the per-site policy and the rules)
//! lives in SQLite and survives restarts.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use pangolin_core::ratelimit::{GLOBAL_SCOPE, IpRule, RateLimitPolicy, RuleKind, normalize_cidr};

use crate::App;
use crate::ok_html_with_csrf;
use crate::routes::helpers::{ok_html, parse_form, redirect};
use crate::templates::{
    RateLimitKpisView, RateLimitPageTemplate, RateLimitRulesView, RateLimitSitesView, RuleRow,
};

type Resp = Response<Full<Bytes>>;

/// Upper bound of a request / page limit.
pub const MAX_LIMIT: u32 = 1_000_000;
/// Upper bound of the custom 429 page (raw HTML), in bytes.
pub const MAX_CUSTOM_429_BYTES: usize = 64 * 1024;
/// Upper bound of a counting window or the block window, in seconds.
pub const MAX_WINDOW_SECS: u64 = 86_400;
/// Upper bound of a ring capacity. Keeps a typo from allocating an
/// unbounded table.
pub const MAX_CAPACITY: u32 = 262_144;

/// `GET /ratelimit` — the full page.
///
/// `error` is the code of a rejected rule submission (see
/// [`add_rule_errors`]); it is rendered as a banner so a typo does not
/// silently drop the rule.
pub async fn render(app: &Arc<App>, csrf: &str, error: Option<&str>) -> http::Result<Resp> {
    let snap = (*app.ratelimit_snapshot()).clone();
    let (rules, site_names) = load_rules_and_sites(app).await;
    let tmpl = RateLimitPageTemplate {
        snap,
        rules: rules.iter().map(RuleRow::new).collect(),
        site_names,
        error: error.map(|code| add_rule_errors(code).to_string()),
        csrf_token: csrf.to_string(),
        active_nav: "ratelimit",
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            log::error!("ratelimit template error: {e}");
            String::new()
        }
    };
    ok_html_with_csrf(html, csrf)
}

/// `GET /api/ratelimit/kpis` — the KPI strip.
pub async fn api_kpis(app: &Arc<App>) -> http::Result<Resp> {
    let snap = (*app.ratelimit_snapshot()).clone();
    let html = RateLimitKpisView { snap }.render().unwrap_or_default();
    ok_html(html)
}

/// `GET /api/ratelimit/sites` — the per-site table and the top
/// over-limit clients.
pub async fn api_sites(app: &Arc<App>) -> http::Result<Resp> {
    let snap = (*app.ratelimit_snapshot()).clone();
    let html = RateLimitSitesView { snap }.render().unwrap_or_default();
    ok_html(html)
}

/// `GET /api/ratelimit/rules` — the IP rules table.
///
/// The fragment carries the delete buttons, so the CSRF placeholder
/// has to be substituted here too — the page polls this fragment
/// every five seconds, and an unsubstituted `__CSRF__` would make
/// every delete button fail the CSRF check.
pub async fn api_rules(app: &Arc<App>, csrf: &str) -> http::Result<Resp> {
    let (rules, _site_names) = load_rules_and_sites(app).await;
    let html = RateLimitRulesView {
        rules: rules.iter().map(RuleRow::new).collect(),
    }
    .render()
    .unwrap_or_default();
    ok_html_with_csrf(html, csrf)
}

/// `GET /api/ratelimit/blocked` — the live "blocked clients" list of
/// the dashboard.
///
/// The fragment carries no buttons, so it needs no CSRF token.
pub async fn api_blocked(app: &Arc<App>) -> http::Result<Resp> {
    let rows = crate::routes::dashboard::blocked_rows(app);
    let html = crate::templates::DashboardBlockedView { blocked: rows }
        .render()
        .unwrap_or_default();
    ok_html(html)
}

/// `POST /ratelimit/rules` — add an IP rule.
///
/// Form POST (not HTMX): the rule table is re-read from the database
/// on the next poll, so a redirect back to the page is enough.
pub async fn handle_add_rule(app: &Arc<App>, body: &[u8], _csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let scope = params
        .get("scope")
        .cloned()
        .unwrap_or_else(|| GLOBAL_SCOPE.to_string());
    let Some(kind) = params.get("kind").and_then(|k| RuleKind::parse(k)) else {
        return Ok(redirect("/ratelimit?error=invalid-kind"));
    };
    let cidr = params.get("cidr").cloned().unwrap_or_default();
    let note = params.get("note").cloned().filter(|n| !n.is_empty());

    // Normalise the address / range and reject a malformed value.
    let normalized = match normalize_cidr(&cidr) {
        Ok(c) => c,
        Err(_) => return Ok(redirect("/ratelimit?error=invalid-cidr")),
    };

    // The scope must be "global" or the name of an existing site — a
    // rule scoped to a site that does not exist would never apply.
    {
        let db = app.db.lock().await;
        let known = scope == GLOBAL_SCOPE
            || pangolin_core::db::list_sites(&db)
                .unwrap_or_default()
                .iter()
                .any(|s| s.name == scope);
        if !known {
            log::warn!("ratelimit: ignoring ip rule for unknown scope {scope:?}");
            return Ok(redirect("/ratelimit?error=unknown-scope"));
        }
    }

    let rule = IpRule {
        id: rule_id(&scope, kind, &normalized),
        scope,
        kind,
        cidr: normalized,
        note,
        created_at: chrono::Utc::now(),
    };

    {
        let db = app.db.lock().await;
        let _ = pangolin_core::db::upsert_ip_rule(&db, &rule);
    }
    app.reload_indexes().await;
    Ok(redirect("/ratelimit"))
}

/// `DELETE /api/ratelimit/rules?scope=…&kind=…&cidr=…` — remove an
/// IP rule.
///
/// The rule is identified by its natural key, so the CIDR travels in
/// the query string (a `/` is legal there — no path-encoding games).
pub async fn api_handle_delete_rule(
    app: &Arc<App>,
    scope: String,
    kind: String,
    cidr: String,
    _csrf: &str,
) -> http::Result<Resp> {
    let Some(kind) = RuleKind::parse(&kind) else {
        return Ok(crate::not_found());
    };
    let Ok(normalized) = normalize_cidr(&cidr) else {
        return Ok(crate::not_found());
    };
    let id = rule_id(&scope, kind, &normalized);
    {
        let db = app.db.lock().await;
        let _ = pangolin_core::db::delete_ip_rule(&db, &id);
    }
    app.reload_indexes().await;
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}

/// Read the IP rules and the site names (for the scope picker).
async fn load_rules_and_sites(app: &Arc<App>) -> (Vec<IpRule>, Vec<String>) {
    let db = app.db.lock().await;
    let rules = pangolin_core::db::list_ip_rules(&db).unwrap_or_default();
    let site_names = pangolin_core::db::list_sites(&db)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.name)
        .collect();
    (rules, site_names)
}

/// The natural key of a rule.
pub fn rule_id(scope: &str, kind: RuleKind, cidr: &str) -> String {
    format!("{}|{}|{}", scope, kind.as_str(), cidr)
}

/// The outcome of parsing the rate-limit fields of the site form.
pub struct ParsedPolicy {
    /// The policy. Always filled — a field that could not be parsed
    /// falls back to its default so the operator does not have to
    /// re-type the whole form after fixing one value.
    pub policy: RateLimitPolicy,
    /// The first error, if any field was malformed.
    pub error: Option<String>,
    /// False when the form carried no rate-limit field at all — an
    /// API client that predates the feature must keep working, and an
    /// update must not silently reset a policy the operator set
    /// through the UI.
    pub present: bool,
}

/// Parse the rate-limit fields of the site form.
///
/// A field that is present but malformed is reported in
/// [`ParsedPolicy::error`]; the other fields are still parsed, so the
/// re-rendered form keeps the operator's input.
pub fn parse_policy(params: &HashMap<String, String>) -> ParsedPolicy {
    const FIELDS: [&str; 9] = [
        "rate_limit_enabled",
        "rate_limit_req_limit",
        "rate_limit_req_window_secs",
        "rate_limit_page_limit",
        "rate_limit_page_window_secs",
        "rate_limit_ip_capacity",
        "rate_limit_throttle_capacity",
        "rate_limit_throttle_secs",
        CUSTOM_429_FIELD,
    ];
    if !FIELDS.iter().any(|f| params.contains_key(*f)) {
        return ParsedPolicy {
            policy: RateLimitPolicy::default(),
            error: None,
            present: false,
        };
    }
    let mut error: Option<String> = None;
    let defaults = RateLimitPolicy::default();

    let enabled = match parse_enabled(params) {
        Ok(v) => v,
        Err(e) => {
            error = Some(e);
            false
        }
    };
    let req_limit = parse_bounded(
        params,
        "rate_limit_req_limit",
        defaults.req_limit,
        0,
        MAX_LIMIT,
        &mut error,
    );
    let req_window_secs = parse_bounded(
        params,
        "rate_limit_req_window_secs",
        defaults.req_window_secs,
        1,
        MAX_WINDOW_SECS,
        &mut error,
    );
    let page_limit = parse_bounded(
        params,
        "rate_limit_page_limit",
        defaults.page_limit,
        0,
        MAX_LIMIT,
        &mut error,
    );
    let page_window_secs = parse_bounded(
        params,
        "rate_limit_page_window_secs",
        defaults.page_window_secs,
        1,
        MAX_WINDOW_SECS,
        &mut error,
    );
    let ip_capacity = parse_bounded(
        params,
        "rate_limit_ip_capacity",
        defaults.ip_capacity,
        1,
        MAX_CAPACITY,
        &mut error,
    );
    let throttle_capacity = parse_bounded(
        params,
        "rate_limit_throttle_capacity",
        defaults.throttle_capacity,
        1,
        MAX_CAPACITY,
        &mut error,
    );
    let throttle_secs = parse_bounded(
        params,
        "rate_limit_throttle_secs",
        defaults.throttle_secs,
        1,
        MAX_WINDOW_SECS,
        &mut error,
    );
    let custom_429_body = match parse_custom_429(params) {
        Ok(v) => v,
        Err(e) => {
            if error.is_none() {
                error = Some(e);
            }
            None
        }
    };

    ParsedPolicy {
        policy: RateLimitPolicy {
            enabled,
            req_limit,
            req_window_secs,
            page_limit,
            page_window_secs,
            ip_capacity,
            throttle_capacity,
            throttle_secs,
            custom_429_body,
        },
        error,
        present: true,
    }
}

/// Read the "enable the limits" flag.
///
/// A browser checkbox submits `on`; an API client may send `true`,
/// `1` or `yes`. Anything else is an error — treating the mere
/// presence of the key as "true" would silently *enable* the limits
/// for a client that meant to disable them.
fn parse_enabled(params: &HashMap<String, String>) -> Result<bool, String> {
    let Some(raw) = params.get("rate_limit_enabled") else {
        return Ok(false);
    };
    let value = raw.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "on" | "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!(
            "rate_limit_enabled must be a boolean, got {other:?}"
        )),
    }
}

/// The form field that carries the site's custom 429 page.
pub const CUSTOM_429_FIELD: &str = "rate_limit_custom_429_body";

/// The custom page exactly as the operator typed it — used to refill
/// the textarea when the submission is rejected for another reason.
pub fn submitted_custom_429(params: &HashMap<String, String>) -> Option<String> {
    params
        .get(CUSTOM_429_FIELD)
        .map(|raw| raw.trim().to_string())
        .filter(|body| !body.is_empty())
}

/// Read the custom 429 page out of the form.
///
/// The body is raw HTML written by the operator — it is served
/// verbatim, exactly like the error page of a plain reverse proxy.
/// An empty (or whitespace-only) body means "use the built-in
/// default page".
fn parse_custom_429(params: &HashMap<String, String>) -> Result<Option<String>, String> {
    let Some(raw) = params.get(CUSTOM_429_FIELD) else {
        return Ok(None);
    };
    let body = raw.trim();
    if body.is_empty() {
        return Ok(None);
    }
    if body.len() > MAX_CUSTOM_429_BYTES {
        return Err(format!(
            "{CUSTOM_429_FIELD} must be at most {MAX_CUSTOM_429_BYTES} bytes"
        ));
    }
    Ok(Some(body.to_string()))
}

/// Read a numeric form field and bound it.
///
/// A missing (or empty) field falls back to `default`; a field that
/// is present but out of range is reported in `error` and falls back
/// to `default` as well.
fn parse_bounded<T>(
    params: &HashMap<String, String>,
    key: &str,
    default: T,
    min: T,
    max: T,
    error: &mut Option<String>,
) -> T
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display + Copy,
{
    let Some(raw) = params.get(key) else {
        return default;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    let Ok(value) = raw.parse::<T>() else {
        if error.is_none() {
            *error = Some(format!("{key} must be a number between {min} and {max}"));
        }
        return default;
    };
    if value < min || value > max {
        if error.is_none() {
            *error = Some(format!("{key} must be between {min} and {max}"));
        }
        return default;
    }
    value
}

/// The messages behind the `?error=` codes of the "add rule" form.
fn add_rule_errors(code: &str) -> &'static str {
    match code {
        "invalid-cidr" => {
            "The address or CIDR range could not be parsed — use 1.2.3.4 or 10.0.0.0/8."
        }
        "invalid-kind" => "The rule kind must be either whitelist or blacklist.",
        "unknown-scope" => "The scope must be \"global\" or the name of an existing site.",
        _ => "The rule could not be added.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pangolin_core::CertManager;
    use pangolin_core::config::Config;
    use tempfile::TempDir;

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn make_test_app() -> (TempDir, Arc<App>) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let app = Arc::new(
            App::new(
                db_path.to_str().expect("utf8"),
                Config::default(),
                CertManager::default(),
            )
            .expect("App::new"),
        );
        (dir, app)
    }

    #[tokio::test]
    async fn page_renders_the_kpis_and_the_rules() {
        let (_dir, app) = make_test_app();
        let resp = render(&app, "csrf", None).await.expect("render");
        assert_eq!(resp.status(), 200);
        let body = resp.into_body();
        let bytes = http_body_util::BodyExt::collect(body)
            .await
            .expect("body")
            .to_bytes();
        let html = String::from_utf8_lossy(&bytes).into_owned();
        assert!(html.contains("Rate Limit"), "{html}");
        assert!(html.contains("/api/ratelimit/kpis"), "{html}");
        assert!(html.contains("/api/ratelimit/rules"), "{html}");
        assert!(html.contains("Add an IP rule"), "{html}");
    }

    #[tokio::test]
    async fn adding_a_rule_persists_it_and_a_delete_removes_it() {
        let (_dir, app) = make_test_app();
        let body = b"scope=global&kind=blacklist&cidr=203.0.113.0%2F24&note=scrapers";
        let resp = handle_add_rule(&app, body, "csrf").await.expect("add rule");
        assert_eq!(resp.status(), 302);

        {
            let db = app.db.lock().await;
            let rules = pangolin_core::db::list_ip_rules(&db).unwrap();
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].scope, "global");
            assert_eq!(rules[0].kind, RuleKind::Blacklist);
            assert_eq!(rules[0].cidr, "203.0.113.0/24");
            assert_eq!(rules[0].note.as_deref(), Some("scrapers"));
        }

        // The rule reaches the index (the global rule applies to
        // every site).
        let resp = api_handle_delete_rule(
            &app,
            "global".into(),
            "blacklist".into(),
            "203.0.113.0/24".into(),
            "csrf",
        )
        .await
        .expect("delete rule");
        assert_eq!(resp.status(), 200);
        let db = app.db.lock().await;
        assert!(pangolin_core::db::list_ip_rules(&db).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_malformed_cidr_is_not_stored() {
        let (_dir, app) = make_test_app();
        let body = b"scope=global&kind=blacklist&cidr=not-an-ip";
        let resp = handle_add_rule(&app, body, "csrf").await.expect("add rule");
        assert_eq!(resp.status(), 302);
        let db = app.db.lock().await;
        assert!(pangolin_core::db::list_ip_rules(&db).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rule_for_an_unknown_scope_is_not_stored() {
        // A rule scoped to a site that does not exist would never
        // apply — the form only offers the existing sites.
        let (_dir, app) = make_test_app();
        let body = b"scope=ghost&kind=blacklist&cidr=203.0.113.0/24";
        let resp = handle_add_rule(&app, body, "csrf").await.expect("add rule");
        assert_eq!(resp.status(), 302);
        let db = app.db.lock().await;
        assert!(pangolin_core::db::list_ip_rules(&db).unwrap().is_empty());
    }

    #[test]
    fn policy_parses_the_form_fields() {
        let params = form(&[
            ("rate_limit_enabled", "on"),
            ("rate_limit_req_limit", "100"),
            ("rate_limit_req_window_secs", "60"),
            ("rate_limit_page_limit", "500"),
            ("rate_limit_page_window_secs", "3600"),
            ("rate_limit_ip_capacity", "16384"),
            ("rate_limit_throttle_capacity", "65536"),
            ("rate_limit_throttle_secs", "600"),
        ]);
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_none(), "{:?}", parsed.error);
        assert!(parsed.present);
        assert!(parsed.policy.enabled);
        assert_eq!(parsed.policy.req_limit, 100);
        assert_eq!(parsed.policy.page_limit, 500);
        assert_eq!(parsed.policy.ip_capacity, 16384);
    }

    #[test]
    fn policy_rejects_out_of_range_values() {
        let base = vec![
            ("rate_limit_req_limit", "1"),
            ("rate_limit_req_window_secs", "60"),
            ("rate_limit_page_limit", "1"),
            ("rate_limit_page_window_secs", "60"),
            ("rate_limit_ip_capacity", "1024"),
            ("rate_limit_throttle_capacity", "1024"),
            ("rate_limit_throttle_secs", "60"),
        ];
        let mut params = form(&base);
        params.insert("rate_limit_ip_capacity".into(), "999999999".into());
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_some());
        // The fields that parsed are kept — the operator only has to
        // fix the one that was wrong.
        assert_eq!(parsed.policy.req_limit, 1);

        let mut params = form(&base);
        params.insert("rate_limit_req_window_secs".into(), "0".into());
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_some());

        let mut params = form(&base);
        params.insert("rate_limit_req_limit".into(), "nope".into());
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_some());
    }

    #[test]
    fn the_custom_429_page_is_read_from_the_form() {
        let mut params = form(&[
            ("rate_limit_req_limit", "10"),
            ("rate_limit_custom_429_body", "<h1>Slow down</h1>"),
        ]);
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_none(), "{:?}", parsed.error);
        assert_eq!(
            parsed.policy.custom_429_body.as_deref(),
            Some("<h1>Slow down</h1>")
        );

        // An empty body means "use the built-in default page".
        params.insert("rate_limit_custom_429_body".into(), "   ".into());
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_none(), "{:?}", parsed.error);
        assert_eq!(parsed.policy.custom_429_body, None);

        // An oversized body is rejected.
        params.insert(
            "rate_limit_custom_429_body".into(),
            "x".repeat(MAX_CUSTOM_429_BYTES + 1),
        );
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_some());
    }

    #[test]
    fn a_form_without_any_rate_limit_field_is_ignored() {
        // An API client that predates the feature must keep working:
        // no field → no policy → the existing one is left alone.
        let params = form(&[("name", "site")]);
        let parsed = parse_policy(&params);
        assert!(!parsed.present);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn policy_without_the_checkbox_is_disabled() {
        let params = form(&[
            ("rate_limit_req_limit", "1"),
            ("rate_limit_req_window_secs", "60"),
            ("rate_limit_page_limit", "1"),
            ("rate_limit_page_window_secs", "60"),
            ("rate_limit_ip_capacity", "1024"),
            ("rate_limit_throttle_capacity", "1024"),
            ("rate_limit_throttle_secs", "60"),
        ]);
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_none(), "{:?}", parsed.error);
        assert!(!parsed.policy.enabled);
    }

    #[test]
    fn cidr_is_normalized() {
        assert_eq!(normalize_cidr("10.0.0.0/8").unwrap(), "10.0.0.0/8");
        assert_eq!(normalize_cidr("1.2.3.4").unwrap(), "1.2.3.4");
        assert!(normalize_cidr("nope").is_err());
    }

    #[test]
    fn rule_id_is_the_natural_key() {
        assert_eq!(
            rule_id("global", RuleKind::Blacklist, "10.0.0.0/8"),
            "global|blacklist|10.0.0.0/8"
        );
    }
}
