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

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use pangolin_core::ratelimit::{
    GLOBAL_SCOPE, IpRule, RateLimitPolicy, RateLimitSnapshot, RuleKind, normalize_cidr,
};

use crate::App;
use crate::ok_html_with_csrf;
use crate::routes::helpers::{ok_html, parse_form, redirect};
use crate::templates::{
    RateLimitKpisView, RateLimitPageTemplate, RateLimitRulesView, RateLimitSitesView, RuleRow,
    TopOverRow,
};

type Resp = Response<Full<Bytes>>;

/// Upper bound of a request / page limit.
pub const MAX_LIMIT: u32 = 1_000_000;
/// Upper bound of the custom 429 page (raw HTML), in bytes.
pub const MAX_CUSTOM_429_BYTES: usize = pangolin_core::ratelimit::MAX_CUSTOM_PAGE_BYTES;
/// Upper bound of the custom 403 page (raw HTML), in bytes.
///
/// The same limit as the 429 page — the two are siblings.
pub const MAX_CUSTOM_403_BYTES: usize = pangolin_core::ratelimit::MAX_CUSTOM_PAGE_BYTES;
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
        top_over: top_over_rows(&snap),
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
    let html = RateLimitSitesView {
        top_over: top_over_rows(&snap),
        snap,
    }
    .render()
    .unwrap_or_default();
    ok_html(html)
}

/// The rows of the "top over-limit clients" table.
///
/// Shared by the page and the HTMX fragment, so both render the same
/// rows. The relative times are taken against one `now`, so every row
/// of a render carries the same clock.
fn top_over_rows(snap: &RateLimitSnapshot) -> Vec<TopOverRow> {
    let now = chrono::Utc::now();
    snap.top_over
        .iter()
        .map(|client| TopOverRow::new(client, now))
        .collect()
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
    const FIELDS: [&str; 10] = [
        "rate_limit_enabled",
        "rate_limit_req_limit",
        "rate_limit_req_window_secs",
        "rate_limit_page_limit",
        "rate_limit_page_window_secs",
        "rate_limit_ip_capacity",
        "rate_limit_throttle_capacity",
        "rate_limit_throttle_secs",
        CUSTOM_429_FIELD,
        CUSTOM_403_FIELD,
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
    let custom_403_body = match parse_custom_403(params) {
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
            custom_403_body,
        },
        error,
        present: true,
    }
}

/// The prefix of the per-bot form fields of the site's bot-limited
/// list (issue #131).
///
/// One field per bot, because the admin's form parser keeps a single
/// value per key — a group of checkboxes sharing one name would lose
/// all but the last value.
pub const BOT_LIMIT_FIELD_PREFIX: &str = "rate_limit_bot_";

/// The hidden marker that says "this submission carries the bot list".
///
/// An unchecked checkbox sends nothing, so "no bot field at all" would
/// be ambiguous between "the operator un-checked everything" and "the
/// submission did not carry the list". The marker resolves it: when it
/// is present the submitted ids replace the site's list — an empty set
/// included; when it is absent the stored list is kept, exactly like
/// the custom 429 / 403 page bodies behave today.
///
/// The name deliberately does **not** start with
/// [`BOT_LIMIT_FIELD_PREFIX`], so the marker is never mistaken for a
/// bot id.
pub const BOT_LIMIT_APPLIED_FIELD: &str = "rate_limit_bots_applied";

/// The known bots the site form offers as checkboxes (issue #131).
///
/// Only the bots whose registry id is safe as a form field name are
/// offered: an id from a hand-written YAML could otherwise produce a
/// field name the browser would not submit back under the same key.
pub fn known_bot_options(app: &Arc<App>) -> Vec<pangolin_core::bot::BotInfo> {
    pangolin_core::bot::registry(app.bot_verifier.as_deref())
        .into_iter()
        .filter(|b| pangolin_core::bot::is_safe_bot_id(&b.name))
        .collect()
}

/// The registry ids the site form offers as checkboxes.
///
/// The set the form can show — and therefore the set the operator can
/// manage through it. A stored id outside it survives a save (see
/// [`merge_bot_list`]): the operator cannot tick or untick it, so
/// saving must not silently drop it.
pub fn renderable_bot_ids(app: &Arc<App>) -> BTreeSet<String> {
    known_bot_options(app).into_iter().map(|b| b.name).collect()
}

/// Merge a submitted bot list with the stored one.
///
/// The submitted ids replace the stored list — except for the stored
/// ids the form could not show: a bot whose registry id is not safe
/// as a form field name is never rendered, so the operator has no way
/// to manage it through the form, and an unrelated save (changing the
/// request limit, say) must not silently drop it.
///
/// The stored ids the form *can* show follow the form: unticking a
/// box removes the bot.
pub fn merge_bot_list(
    submitted: &BTreeSet<String>,
    stored: &BTreeSet<String>,
    renderable: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut out = submitted.clone();
    for id in stored {
        if !renderable.contains(id) {
            out.insert(id.clone());
        }
    }
    out
}

/// Whether a form value means "on".
///
/// The same table the "enable the limits" checkbox uses: a browser
/// sends `on`, an API client may send `true`, `1` or `yes`, and an
/// empty value counts as "the field was there" (a checkbox that
/// carries no value is still a checked checkbox).
fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "on" | "true" | "1" | "yes"
    )
}

/// Parse the site's bot-limited list out of the submitted form.
///
/// `None` means the form carried no list at all — the stored list is
/// then left untouched. `Some(set)` means the submitted ids replace
/// the site's list; an empty set empties it.
///
/// The marker field's **value** decides, not its mere presence: a
/// client that sends `rate_limit_bots_applied=0` meant "not applied",
/// and reading it as "applied" would empty the site's list. The same
/// goes for a per-bot field — `rate_limit_bot_gptbot=0` means "not
/// checked", not "checked".
///
/// An id that is not safe as a form field name is dropped: only
/// `A-Z`, `a-z`, `0-9`, `-` and `_` are accepted. An id the registry
/// does not know is kept — it can never match a request, because only
/// a bot the verifier knows can produce a verified bot.
pub fn parse_bot_limits(params: &HashMap<String, String>) -> Option<BTreeSet<String>> {
    if !params
        .get(BOT_LIMIT_APPLIED_FIELD)
        .is_some_and(|v| is_truthy(v))
    {
        return None;
    }
    let mut bots = BTreeSet::new();
    for (key, value) in params {
        let Some(id) = key.strip_prefix(BOT_LIMIT_FIELD_PREFIX) else {
            continue;
        };
        if !is_truthy(value) {
            continue;
        }
        if !pangolin_core::bot::is_safe_bot_id(id) {
            continue;
        }
        bots.insert(id.to_string());
    }
    Some(bots)
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
    if is_truthy(&value) {
        return Ok(true);
    }
    match value.as_str() {
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!(
            "rate_limit_enabled must be a boolean, got {other:?}"
        )),
    }
}

/// The form field that carries the site's custom 429 page.
pub const CUSTOM_429_FIELD: &str = "rate_limit_custom_429_body";

/// The form field that carries the site's custom 403 page.
pub const CUSTOM_403_FIELD: &str = "custom_403_body";

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

/// Read the custom 403 page out of the form.
///
/// The body is raw HTML written by the operator — it is served
/// verbatim for every `403` the gateway itself produces (the origin
/// guard and the IP-blacklist rule). An empty (or whitespace-only)
/// body means "use the built-in default page".
fn parse_custom_403(params: &HashMap<String, String>) -> Result<Option<String>, String> {
    let Some(raw) = params.get(CUSTOM_403_FIELD) else {
        return Ok(None);
    };
    let body = raw.trim();
    if body.is_empty() {
        return Ok(None);
    }
    if body.len() > MAX_CUSTOM_403_BYTES {
        return Err(format!(
            "{CUSTOM_403_FIELD} must be at most {MAX_CUSTOM_403_BYTES} bytes"
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
    use pangolin_core::ratelimit::{ClientGeo, LimitDecision};
    use pangolin_core::types::{HostMode, Site};
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

    /// A site whose request limit is 1, so the second request of a
    /// client is throttled.
    async fn seed_limited_site(app: &Arc<App>) {
        let site = Site {
            name: "a".into(),
            backend: "http://127.0.0.1:8080".into(),
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        let policy = RateLimitPolicy {
            enabled: true,
            req_limit: 1,
            req_window_secs: 60,
            page_limit: 0,
            page_window_secs: 3600,
            ip_capacity: 256,
            throttle_capacity: 256,
            throttle_secs: 600,
            custom_429_body: None,
            custom_403_body: None,
        };
        {
            let db = app.db.lock().await;
            pangolin_core::db::upsert_site(&db, &site).unwrap();
            pangolin_core::db::upsert_site_rate_limit(&db, "a", &policy).unwrap();
        }
        app.reload_indexes().await;
    }

    /// Drive a client over the request limit of the seeded site.
    async fn throttle(app: &Arc<App>, client: std::net::IpAddr, geo: ClientGeo<'_>) {
        let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
        assert_eq!(
            app.rate_limiter
                .check("a", &cfg, Some(client), None, Some("/"), geo),
            LimitDecision::Allow
        );
        assert!(
            matches!(
                app.rate_limiter
                    .check("a", &cfg, Some(client), None, Some("/"), geo),
                LimitDecision::Throttled { .. }
            ),
            "the client must be throttled"
        );
    }

    /// A site whose request limit is 1 **and** whose bot-limited list
    /// holds `googlebot` — so a request that carries that bot
    /// identity is counted like any other client and is throttled
    /// (issue #131).
    async fn seed_bot_limited_site(app: &Arc<App>) {
        seed_limited_site(app).await;
        {
            let db = app.db.lock().await;
            pangolin_core::db::set_site_rate_limit_bots(
                &db,
                "a",
                &BTreeSet::from(["googlebot".to_string()]),
            )
            .unwrap();
        }
        app.reload_indexes().await;
    }

    /// Drive a client that claims a bot identity over the request
    /// limit of the seeded site.
    async fn throttle_as_bot(app: &Arc<App>, client: std::net::IpAddr, bot: &str) {
        let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
        assert_eq!(
            app.rate_limiter.check(
                "a",
                &cfg,
                Some(client),
                Some(bot),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(
            matches!(
                app.rate_limiter.check(
                    "a",
                    &cfg,
                    Some(client),
                    Some(bot),
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Throttled { .. }
            ),
            "the bot must be throttled"
        );
    }

    /// Wait until the *published* snapshot carries the rejected client
    /// with the expected number of rejections.
    ///
    /// The publisher thread rebuilds the snapshot once per second, so
    /// a render right after the rejection can still miss it.
    async fn wait_for_published(app: &Arc<App>, ip: &str, rejections: u64) {
        for _ in 0..50 {
            let snap = app.ratelimit_snapshot();
            let row = snap.top_over.iter().find(|c| c.ip == ip);
            if let Some(row) = row
                && row.rule_blocks + row.throttled == rejections
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the rejected client never reached the published snapshot");
    }

    /// The body of a response, as a string.
    async fn html_of(resp: Response<Full<Bytes>>) -> String {
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The column labels of the table that follows `marker`, in order.
    ///
    /// `marker` is a heading of the table — the head of the table is
    /// the first `<thead` *after* it.
    fn table_headers(html: &str, marker: &str) -> Vec<String> {
        let at = html
            .find(marker)
            .unwrap_or_else(|| panic!("no {marker} in the fragment"));
        let thead = html[at..]
            .find("<thead")
            .map(|i| at + i)
            .expect("the table has a head");
        let head = &html[thead..];
        let end = head.find("</thead>").expect("a closed table head");
        head[..end]
            .split("<th ")
            .skip(1)
            .map(|cell| {
                let open = cell.find('>').expect("a closed header cell");
                let close = cell[open..].find("</th>").expect("a closed header cell");
                cell[open + 1..open + close].trim().to_string()
            })
            .collect()
    }

    /// The markup of the `<tr>` that carries `marker`.
    fn row_with<'a>(html: &'a str, marker: &str) -> &'a str {
        let at = html
            .find(marker)
            .unwrap_or_else(|| panic!("no row with {marker} in the fragment"));
        let start = html[..at].rfind("<tr").expect("the row opens with a <tr>");
        let end = html[at..].find("</tr>").expect("the row is closed") + at;
        &html[start..end]
    }

    /// The cells of a row, in order.
    fn cells_of(row: &str) -> Vec<&str> {
        let mut cells = Vec::new();
        let mut rest = row;
        while let Some(open) = rest.find("<td") {
            let after = &rest[open..];
            let close = after.find("</td>").expect("the cell is closed");
            cells.push(&after[..close]);
            rest = &after[close..];
        }
        cells
    }

    #[tokio::test]
    async fn the_over_limit_table_shows_the_geo_the_status_and_the_first_block_time() {
        let (_dir, app) = make_test_app();
        seed_limited_site(&app).await;
        let client: std::net::IpAddr = "198.51.100.9".parse().unwrap();
        throttle(
            &app,
            client,
            ClientGeo {
                country: Some("JP"),
                region: Some("13"),
            },
        )
        .await;
        wait_for_published(&app, &client.to_string(), 1).await;

        let html = html_of(api_sites(&app).await.expect("api_sites")).await;
        assert!(html.contains("data-ip=\"198.51.100.9\""), "{html}");
        assert!(html.contains("data-country=\"JP\""), "{html}");
        assert!(html.contains("data-region=\"13\""), "{html}");
        // The status badge — the client was rejected with 429.
        assert!(html.contains(">429</span>"), "{html}");
        // The total number of rejections. The attribute is checked,
        // not the cell text: the per-site table renders its 429 cell
        // with the very same classes, so the text alone would also
        // match that cell.
        assert!(html.contains("data-rejections=\"1\""), "{html}");
        // The relative time of the first rejection ("0s ago", "1s ago", …).
        assert!(html.contains("s ago"), "{html}");
        // The absolute timestamp stays available in the cell's title.
        // The *last* title of the fragment is the one of the
        // first-blocked cell — the lookup button of the client-IP
        // cell carries a title of its own, and it comes first.
        let at = html
            .rfind("title=\"")
            .expect("the first-blocked cell has a title");
        let stamp = &html[at + "title=\"".len()..];
        let end = stamp.find('"').expect("the title attribute is closed");
        let stamp = &stamp[..end];
        let parsed = chrono::DateTime::parse_from_rfc3339(stamp)
            .expect("the title carries an RFC3339 timestamp");
        let drift = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_seconds();
        assert!(
            drift.abs() <= 60,
            "the title must be the moment of the first rejection: {stamp}"
        );

        // The full page carries the same table — it includes the
        // fragment, so the columns are there on the first paint too.
        let page = html_of(render(&app, "csrf", None).await.expect("render")).await;
        for column in ["Country", "Region", "Status", "Rejections", "First blocked"] {
            assert!(
                page.contains(column),
                "the page must have a {column} column: {page}"
            );
        }
        assert!(page.contains(">429</span>"), "{page}");
    }

    #[tokio::test]
    async fn a_client_without_a_geo_renders_an_em_dash() {
        let (_dir, app) = make_test_app();
        seed_limited_site(&app).await;
        let client: std::net::IpAddr = "198.51.100.10".parse().unwrap();
        throttle(&app, client, ClientGeo::default()).await;
        wait_for_published(&app, &client.to_string(), 1).await;

        let html = html_of(api_sites(&app).await.expect("api_sites")).await;
        assert!(html.contains("data-country=\"\""), "{html}");
        assert!(html.contains("data-region=\"\""), "{html}");
        // The country and the region cells render an em dash. The
        // cells are checked one by one — the Bot column renders an em
        // dash too, so a plain count over the whole fragment would
        // not say *which* cells are empty.
        let row = row_with(&html, "data-ip=\"198.51.100.10\"");
        let cells = cells_of(row);
        // Site, Client IP, Bot, Country, Region, …
        assert!(cells[3].contains('—'), "{row}");
        assert!(cells[4].contains('—'), "{row}");
    }

    /// A throttled known bot is shown with its name, and its client-IP
    /// cell carries the lookup hooks (issue #134, R3–R6, R13).
    #[tokio::test]
    async fn the_over_limit_table_shows_the_bot_name_and_the_lookup_hooks() {
        let (_dir, app) = make_test_app();
        seed_bot_limited_site(&app).await;
        let client: std::net::IpAddr = "198.51.100.40".parse().unwrap();
        throttle_as_bot(&app, client, "googlebot").await;
        wait_for_published(&app, &client.to_string(), 1).await;

        let html = html_of(api_sites(&app).await.expect("api_sites")).await;
        // The column order — the Bot column sits right after the
        // client IP (R3).
        assert_eq!(
            table_headers(&html, "Top over-limit clients"),
            vec![
                "Site",
                "Client IP",
                "Bot",
                "Country",
                "Region",
                "Status",
                "Rejections",
                "First blocked"
            ]
        );

        // The row carries the name (R5) and renders it as a blue chip
        // (R4).
        let row = row_with(&html, "data-bot=\"googlebot\"");
        assert!(row.contains("bg-blue-100"), "{row}");
        assert!(row.contains(">googlebot</span>"), "{row}");

        // The client-IP cell carries the lookup hooks and the two
        // result placeholders (R6, R13).
        let cell = cells_of(row)[1];
        assert!(cell.contains("data-asn-ip=\"198.51.100.40\""), "{cell}");
        assert_eq!(cell.matches("data-asn-trigger").count(), 1, "{cell}");
        assert!(cell.contains("data-asn-geo"), "{cell}");
        assert!(cell.contains("data-asn-asn"), "{cell}");
        // The tooltip says that the address leaves the operator's
        // browser (R11).
        assert!(
            cell.contains("sends this address to ipwhois.app / ipinfo.io"),
            "{cell}"
        );
    }

    /// A client that is not a verified known bot has no name — the Bot
    /// column renders a muted em dash and no chip (issue #134, R4).
    #[tokio::test]
    async fn a_client_without_a_bot_renders_an_em_dash_in_the_bot_column() {
        let (_dir, app) = make_test_app();
        seed_limited_site(&app).await;
        let client: std::net::IpAddr = "198.51.100.41".parse().unwrap();
        throttle(&app, client, ClientGeo::default()).await;
        wait_for_published(&app, &client.to_string(), 1).await;

        let html = html_of(api_sites(&app).await.expect("api_sites")).await;
        let row = row_with(&html, "data-bot=\"\"");
        assert!(!row.contains("bg-blue-100"), "{row}");
        // Site, Client IP, Bot, …
        let cells = cells_of(row);
        assert!(cells[2].contains('—'), "{row}");
        assert!(!cells[2].contains("bg-blue-100"), "{row}");
    }

    /// The dashboard's live "blocked clients" table shows the bot name
    /// and the lookup hooks too (issue #134, R3–R6).
    #[tokio::test]
    async fn the_dashboard_blocked_table_shows_the_bot_column() {
        let (_dir, app) = make_test_app();
        seed_bot_limited_site(&app).await;
        let client: std::net::IpAddr = "198.51.100.42".parse().unwrap();
        throttle_as_bot(&app, client, "googlebot").await;
        wait_for_published(&app, &client.to_string(), 1).await;

        let html = html_of(api_blocked(&app).await.expect("api_blocked")).await;
        // The column order — the Bot column sits right after the
        // client IP (R3).
        assert_eq!(
            table_headers(&html, "Blocked clients"),
            vec!["Client IP", "Bot", "Site", "403", "429", "Last seen"]
        );

        let row = row_with(&html, "data-bot=\"googlebot\"");
        assert!(row.contains("bg-blue-100"), "{row}");
        assert!(row.contains(">googlebot</span>"), "{row}");

        // The client-IP cell carries the lookup hooks (R6).
        let cell = cells_of(row)[0];
        assert!(cell.contains("data-asn-ip=\"198.51.100.42\""), "{cell}");
        assert_eq!(cell.matches("data-asn-trigger").count(), 1, "{cell}");
        assert!(cell.contains("data-asn-geo"), "{cell}");
        assert!(cell.contains("data-asn-asn"), "{cell}");
    }

    /// The empty state of the "top over-limit clients" table spans the
    /// eight columns of the table (issue #134, R3).
    #[tokio::test]
    async fn the_empty_over_limit_table_spans_eight_columns() {
        let (_dir, app) = make_test_app();
        let html = html_of(api_sites(&app).await.expect("api_sites")).await;
        assert!(html.contains("colspan=\"8\""), "{html}");
    }

    #[tokio::test]
    async fn the_status_and_the_rejections_count_both_kinds_of_rejection() {
        let (_dir, app) = make_test_app();
        seed_limited_site(&app).await;
        let client: std::net::IpAddr = "198.51.100.20".parse().unwrap();

        // A blacklisted client is rejected with 403.
        let cidr = client.to_string();
        let rule = IpRule {
            id: rule_id(GLOBAL_SCOPE, RuleKind::Blacklist, &cidr),
            scope: GLOBAL_SCOPE.into(),
            kind: RuleKind::Blacklist,
            cidr: cidr.clone(),
            note: None,
            created_at: chrono::Utc::now(),
        };
        {
            let db = app.db.lock().await;
            pangolin_core::db::upsert_ip_rule(&db, &rule).unwrap();
        }
        app.reload_indexes().await;
        let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
        assert_eq!(
            app.rate_limiter.check(
                "a",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Blocked
        );

        // The rule is removed — the client is now only over the limit.
        {
            let db = app.db.lock().await;
            pangolin_core::db::delete_ip_rule(&db, &rule.id).unwrap();
        }
        app.reload_indexes().await;
        let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
        assert_eq!(
            app.rate_limiter.check(
                "a",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(
            matches!(
                app.rate_limiter.check(
                    "a",
                    &cfg,
                    Some(client),
                    None,
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Throttled { .. }
            ),
            "the client must be throttled"
        );

        wait_for_published(&app, &client.to_string(), 2).await;
        let html = html_of(api_sites(&app).await.expect("api_sites")).await;
        // Both kinds of rejection are counted and shown.
        assert!(html.contains("data-status=\"403+429\""), "{html}");
        assert!(html.contains("data-rejections=\"2\""), "{html}");
        assert!(html.contains(">403+429</span>"), "{html}");
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

    /// A form that carries no bot field at all leaves the stored list
    /// untouched — an API client that predates the feature must not
    /// wipe it (issue #131).
    #[test]
    fn a_form_without_the_bot_list_keeps_the_stored_list() {
        let params = form(&[("rate_limit_req_limit", "10")]);
        assert_eq!(parse_bot_limits(&params), None);
    }

    /// The marker field is what distinguishes "the operator un-checked
    /// everything" from "the submission said nothing about the list".
    #[test]
    fn the_marker_field_empties_the_list() {
        let params = form(&[(BOT_LIMIT_APPLIED_FIELD, "1")]);
        assert_eq!(parse_bot_limits(&params), Some(BTreeSet::new()));
    }

    /// The checked boxes are the site's list.
    #[test]
    fn the_checked_boxes_are_the_bot_list() {
        let params = form(&[
            (BOT_LIMIT_APPLIED_FIELD, "1"),
            ("rate_limit_bot_gptbot", "on"),
            ("rate_limit_bot_ccbot", "on"),
        ]);
        let bots = parse_bot_limits(&params).expect("the form carried the list");
        assert_eq!(
            bots,
            ["ccbot".to_string(), "gptbot".to_string()]
                .into_iter()
                .collect::<BTreeSet<String>>()
        );
    }

    /// An id that is not safe as a form field name is dropped — it
    /// could never have been submitted by the rendered form.
    #[test]
    fn an_unsafe_bot_id_is_dropped() {
        let mut params = form(&[(BOT_LIMIT_APPLIED_FIELD, "1")]);
        params.insert("rate_limit_bot_good".into(), "on".into());
        params.insert("rate_limit_bot_bad id".into(), "on".into());
        params.insert("rate_limit_bot_bad\"id".into(), "on".into());
        let bots = parse_bot_limits(&params).expect("the form carried the list");
        assert_eq!(
            bots,
            ["good".to_string()]
                .into_iter()
                .collect::<BTreeSet<String>>()
        );
    }

    /// The marker's **value** decides, not its mere presence: a client
    /// that sends `rate_limit_bots_applied=0` meant "not applied", and
    /// reading it as "applied" would empty the site's list.
    #[test]
    fn a_falsy_marker_does_not_apply_the_list() {
        let params = form(&[(BOT_LIMIT_APPLIED_FIELD, "0")]);
        assert_eq!(parse_bot_limits(&params), None);
    }

    /// The same goes for a per-bot field: `rate_limit_bot_gptbot=0`
    /// means "not checked", not "checked".
    #[test]
    fn a_falsy_bot_field_is_not_checked() {
        let params = form(&[
            (BOT_LIMIT_APPLIED_FIELD, "1"),
            ("rate_limit_bot_gptbot", "0"),
            ("rate_limit_bot_ccbot", "on"),
        ]);
        let bots = parse_bot_limits(&params).expect("the form carried the list");
        assert_eq!(
            bots,
            ["ccbot".to_string()]
                .into_iter()
                .collect::<BTreeSet<String>>()
        );
    }

    /// The stored ids the form cannot show survive a save: the
    /// operator has no way to manage them through the form, so an
    /// unrelated save must not silently drop them.
    #[test]
    fn the_stored_ids_the_form_cannot_show_survive_a_save() {
        let submitted: BTreeSet<String> = ["gptbot".to_string()].into_iter().collect();
        let stored: BTreeSet<String> = ["gptbot".to_string(), "my.bot".to_string()]
            .into_iter()
            .collect();
        // The form shows `gptbot` (it is in the registry and its id is
        // safe) but not `my.bot`.
        let renderable: BTreeSet<String> = ["gptbot".to_string()].into_iter().collect();
        let merged = merge_bot_list(&submitted, &stored, &renderable);
        assert_eq!(
            merged,
            ["gptbot".to_string(), "my.bot".to_string()]
                .into_iter()
                .collect::<BTreeSet<String>>()
        );
    }

    /// A stored id the form *can* show follows the form: unticking the
    /// box removes the bot.
    #[test]
    fn an_unticked_renderable_bot_is_removed() {
        let submitted: BTreeSet<String> = BTreeSet::new();
        let stored: BTreeSet<String> = ["gptbot".to_string()].into_iter().collect();
        let renderable: BTreeSet<String> = ["gptbot".to_string()].into_iter().collect();
        assert!(merge_bot_list(&submitted, &stored, &renderable).is_empty());
    }

    /// The marker is never mistaken for a bot id — its name does not
    /// start with the per-bot prefix.
    #[test]
    fn the_marker_is_not_a_bot_id() {
        assert!(!BOT_LIMIT_APPLIED_FIELD.starts_with(BOT_LIMIT_FIELD_PREFIX));
        let params = form(&[(BOT_LIMIT_APPLIED_FIELD, "1")]);
        let bots = parse_bot_limits(&params).expect("the form carried the list");
        assert!(bots.is_empty());
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
    fn the_custom_403_page_is_read_from_the_form() {
        let mut params = form(&[
            ("rate_limit_req_limit", "10"),
            ("custom_403_body", "<h1>Blocked</h1>"),
        ]);
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_none(), "{:?}", parsed.error);
        assert_eq!(
            parsed.policy.custom_403_body.as_deref(),
            Some("<h1>Blocked</h1>")
        );

        // An empty body means "use the built-in default page".
        params.insert("custom_403_body".into(), "   ".into());
        let parsed = parse_policy(&params);
        assert!(parsed.error.is_none(), "{:?}", parsed.error);
        assert_eq!(parsed.policy.custom_403_body, None);

        // An oversized body is rejected.
        params.insert(
            "custom_403_body".into(),
            "x".repeat(MAX_CUSTOM_403_BYTES + 1),
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
