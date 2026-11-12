//! Templates for `/ratelimit` and its HTMX fragments.

use askama::Template;
use pangolin_core::ratelimit::{IpRule, OverLimitClient, RateLimitSnapshot};

/// One row of the IP rules table.
///
/// The delete URL is built here (not in the template) so the query
/// values are percent-encoded — a site name that contains a `&`
/// would otherwise split the query string.
#[derive(Clone, Debug)]
pub struct RuleRow {
    pub scope: String,
    pub kind: String,
    pub cidr: String,
    pub note: String,
    /// The URL of the HTMX delete endpoint.
    pub delete_url: String,
}

impl RuleRow {
    /// Build the row of an IP rule.
    pub fn new(rule: &IpRule) -> Self {
        let cidr = urlencoding::encode(&rule.cidr).into_owned();
        let scope = urlencoding::encode(&rule.scope).into_owned();
        let kind = urlencoding::encode(rule.kind.as_str()).into_owned();
        Self {
            scope: rule.scope.clone(),
            kind: rule.kind.as_str().to_string(),
            cidr: rule.cidr.clone(),
            note: rule.note.clone().unwrap_or_default(),
            delete_url: format!("/api/ratelimit/rules?scope={scope}&kind={kind}&cidr={cidr}"),
        }
    }
}

/// One row of the "top over-limit clients" table.
///
/// The display strings are computed here, not in the template (the
/// same pattern as [`RuleRow`]): Askama's expression parser cannot
/// follow `crate::templates::relative_time(...)`, and the status
/// badge colour is keyed off the `status` string in the template —
/// the Tailwind classes have to appear literally in the HTML, or
/// the badge ships unstyled.
#[derive(Clone, Debug)]
pub struct TopOverRow {
    pub site: String,
    pub ip: String,
    /// The country code, `None` when the provider reported none.
    pub country: Option<String>,
    /// The region code, `None` when the provider reported none.
    pub region: Option<String>,
    /// The registry id of the verified known bot the client was
    /// identified as — `None` for a client that is not one (issue
    /// #134).
    pub bot_name: Option<String>,
    /// What the IP reputation library said about the client (issue
    /// #141) — `None` when the library was never asked about it.
    ///
    /// The verdict is rendered as a badge: the badge colour is keyed
    /// off [`Self::badbot_code`], so the Tailwind classes have to
    /// appear literally in the HTML template, or the badge ships
    /// unstyled.
    pub badbot: Option<pangolin_core::BadbotVerdict>,
    /// The verdict's stable code — `bad_address`, `bot_user_agent`,
    /// `bad_address_bot_user_agent` or `unknown`.
    ///
    /// Owned rather than borrowed: the template compares it against a
    /// string literal, and Askama binds a `&str` out of an `Option`
    /// as a `&&str`, which does not compare with a `&str`.
    pub badbot_code: Option<String>,
    /// "403", "429" or "403+429" — the status the client was rejected
    /// with.
    pub status: String,
    /// The total number of rejections (403 + 429).
    pub rejections: u64,
    /// "12m ago" — the relative time of the first rejection.
    pub first_seen_rel: String,
    /// The RFC3339 UTC timestamp of the first rejection.
    pub first_seen_at: String,
}

impl TopOverRow {
    /// Build the row of a rejected client.
    ///
    /// `now` is the reference point of the relative time — the caller
    /// reads the whole table at one instant, so every row of a render
    /// carries the same clock.
    pub fn new(client: &OverLimitClient, now: chrono::DateTime<chrono::Utc>) -> Self {
        let status = match (client.rule_blocks > 0, client.throttled > 0) {
            (true, true) => "403+429",
            (true, false) => "403",
            (false, true) => "429",
            // Unreachable: a row only exists because a rejection was
            // recorded, so one of the two counters is non-zero.
            (false, false) => "",
        };
        Self {
            site: client.site.clone(),
            ip: client.ip.clone(),
            country: client.country.clone(),
            region: client.region.clone(),
            bot_name: client.bot_name.clone(),
            badbot: client.badbot,
            badbot_code: client.badbot.map(|v| v.code().to_string()),
            status: status.to_string(),
            rejections: client.rule_blocks.saturating_add(client.throttled),
            first_seen_rel: super::relative_time(client.first_seen_at, now),
            first_seen_at: client.first_seen_at.to_rfc3339(),
        }
    }
}

/// Full page — KPI cards, per-site table, top over-limit clients and
/// the IP rules, HTMX-refreshed after first paint.
#[derive(Template)]
#[template(path = "pages/ratelimit.html")]
pub struct RateLimitPageTemplate<'a> {
    pub snap: RateLimitSnapshot,
    /// The rows of the "top over-limit clients" table.
    pub top_over: Vec<TopOverRow>,
    pub rules: Vec<RuleRow>,
    /// Site names — the scope picker of the "add rule" form.
    pub site_names: Vec<String>,
    /// The message of a rejected rule submission, if any.
    pub error: Option<String>,
    pub csrf_token: String,
    pub active_nav: &'a str,
}

/// KPI strip swapped every 2s (`GET /api/ratelimit/kpis`).
#[derive(Template)]
#[template(path = "views/ratelimit/_kpis.html")]
pub struct RateLimitKpisView {
    pub snap: RateLimitSnapshot,
}

/// Per-site table + top over-limit clients (`GET /api/ratelimit/sites`).
#[derive(Template)]
#[template(path = "views/ratelimit/_sites.html")]
pub struct RateLimitSitesView {
    pub snap: RateLimitSnapshot,
    /// The rows of the "top over-limit clients" table.
    pub top_over: Vec<TopOverRow>,
}

/// IP rules table (`GET /api/ratelimit/rules`).
#[derive(Template)]
#[template(path = "views/ratelimit/_rules.html")]
pub struct RateLimitRulesView {
    pub rules: Vec<RuleRow>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn client(rule_blocks: u64, throttled: u64) -> OverLimitClient {
        OverLimitClient {
            site: "site".into(),
            ip: "198.51.100.9".into(),
            country: Some("JP".into()),
            region: Some("13".into()),
            bot_name: None,
            badbot: None,
            rule_blocks,
            throttled,
            first_seen_secs: 42,
            first_seen_at: Utc::now(),
        }
    }

    #[test]
    fn the_status_is_derived_from_the_two_counters() {
        let now = Utc::now();
        assert_eq!(TopOverRow::new(&client(3, 0), now).status, "403");
        assert_eq!(TopOverRow::new(&client(0, 7), now).status, "429");
        assert_eq!(TopOverRow::new(&client(3, 7), now).status, "403+429");
    }

    #[test]
    fn the_rejections_are_the_total_of_the_two_counters() {
        let now = Utc::now();
        assert_eq!(TopOverRow::new(&client(3, 7), now).rejections, 10);
        assert_eq!(TopOverRow::new(&client(3, 0), now).rejections, 3);
        assert_eq!(TopOverRow::new(&client(0, 7), now).rejections, 7);
    }

    #[test]
    fn the_first_block_time_is_relative_with_an_absolute_timestamp() {
        let now = Utc::now();
        let mut c = client(0, 7);
        c.first_seen_at = now - Duration::minutes(12);
        let row = TopOverRow::new(&c, now);
        assert_eq!(row.first_seen_rel, "12m ago");
        assert_eq!(row.first_seen_at, c.first_seen_at.to_rfc3339());
    }

    /// The bot name of the client is carried into the row (issue
    /// #134).
    #[test]
    fn the_bot_name_is_carried_into_the_row() {
        let now = Utc::now();
        let mut c = client(0, 7);
        c.bot_name = Some("googlebot".into());
        assert_eq!(
            TopOverRow::new(&c, now).bot_name.as_deref(),
            Some("googlebot")
        );
        // A client that is not a verified known bot has no name.
        assert_eq!(TopOverRow::new(&client(0, 7), now).bot_name, None);
    }
}
