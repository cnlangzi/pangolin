//! Askama templates for admin pages.
//!
//! ## Layout
//!
//! Templates are organized per resource. Each resource has its own module
//! (e.g. `sites`, `domains`, `tun`, `certs`, `dns`) and exports all the
//! `Template` structs needed by the corresponding route handler.
//!
//! File organization under `crates/admin/templates/` mirrors the URL
//! namespace:
//!
//! - `layouts/<name>.html`        — base layouts (extends `title`/`content` blocks)
//! - `components/<name>.html`     — stateless fragments (CSRF input, etc.)
//! - `pages/<resource>/<action>.html`  — full pages (`extends "layouts/base.html"`)
//! - `views/<resource>/<fragment>.html` — non-page templates rendered explicitly
//!
//! ## Naming convention
//!
//! Struct names are aligned to file paths and use the
//! `<Resource><Purpose>Template` pattern, e.g.:
//!
//! - `SitesListTemplate`      → `pages/sites/list.html`
//! - `SitesNewTemplate`       → `pages/sites/new.html`
//! - `SitesEditTemplate`      → `pages/sites/edit.html`
//! - `SitesTableView`         → `views/sites/_table.html`
//! - `SitesFormFieldsView`    → `views/sites/_form_fields.html`
//!
//! (The convention deviates for the auth login page, which is one-off:
//! `LoginTemplate` → `pages/auth/login.html`.)

pub mod auth;
pub mod cache;
pub mod certs;
pub mod dashboard;
pub mod dns;
pub mod domains;
pub mod logs;
pub mod ratelimit;
pub mod settings;
pub mod sites;
pub mod traffic;
pub mod tun;

// Re-export per-resource template structs for ergonomic use at the call site:
// `use crate::templates::SitesListTemplate;`
pub use auth::LoginTemplate;
pub use cache::CacheCardView;
pub use certs::{CertRow, CertsListTemplate, CertsNewTemplate};
pub use dashboard::{ActivityRow, BlockedRow, DashboardBlockedView, DashboardTemplate};
pub use dns::{
    DnsProvidersEditTemplate, DnsProvidersFormFieldsView, DnsProvidersListTemplate,
    DnsProvidersNewTemplate,
};
pub use domains::{
    DomainsEditTemplate, DomainsFormFieldsView, DomainsListTemplate, DomainsNewTemplate,
    DomainsTableView, SiteDomainsTableView, SiteDomainsTemplate,
};
pub use logs::{
    BotHistoryFilter, BotHistorySummary, BotLogsHistoryTemplate, BotLogsStatsTemplate,
    BotLogsSummaryView, BotLogsTemplate, LogsTemplate, RefererHistoryDateLink,
    RefererHistoryFilter, RefererHistoryStatsPage, RefererHistorySummary,
    RefererLogsHistoryTemplate, RefererLogsStatsTemplate, RefererLogsSummaryView,
    RefererLogsTemplate, RefererSiteOption, RefererStatsHostRow, RefererStatsPathRow,
    referer_subnav, subnav,
};
pub use ratelimit::{
    RateLimitKpisView, RateLimitPageTemplate, RateLimitRulesView, RateLimitSitesView, RuleRow,
    TopOverRow,
};
pub use settings::{BadbotsSettingsView, DashboardApiView, ProviderOption, SettingsTemplate};
pub use sites::{
    SitesEditTemplate, SitesFormFieldsView, SitesListTemplate, SitesNewTemplate, SitesTableView,
};
pub use traffic::{TrafficKpisView, TrafficPageTemplate, TrafficTablesView};
pub use tun::{
    TunnelsEditTemplate, TunnelsFormFieldsView, TunnelsListTemplate, TunnelsNewTemplate,
};

/// Format `then` as a coarse "x seconds/minutes/hours/days ago" relative to
/// `now`. Used by `views/certs/_table.html` for the Started column so an
/// operator can tell at a glance how stale a Pending / Failed row is.
///
/// Granularity: the smallest unit whose value is ≥1; future timestamps
/// (clock skew) get an "in the future" string rather than a negative
/// number so the column never reads as garbage.
///
/// The wording comes from the language catalog, so the column reads in
/// the operator's language.
pub fn relative_time(
    then: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
    lang: crate::i18n::Lang,
) -> String {
    let secs = (now - then).num_seconds();
    if secs < 0 {
        return lang.t("time.in_the_future").into_owned();
    }
    if secs < 60 {
        return lang.tf("time.seconds_ago", [("n", secs)]);
    }
    let mins = secs / 60;
    if mins < 60 {
        return lang.tf("time.minutes_ago", [("n", mins)]);
    }
    let hours = mins / 60;
    if hours < 24 {
        return lang.tf("time.hours_ago", [("n", hours)]);
    }
    let days = hours / 24;
    lang.tf("time.days_ago", [("n", days)])
}

#[cfg(test)]
mod tests {
    use super::relative_time;
    use crate::i18n::Lang;
    use chrono::{Duration, Utc};

    #[test]
    fn relative_time_buckets() {
        let now = Utc::now();
        // The English wording must not change.
        assert_eq!(
            relative_time(now - Duration::seconds(5), now, Lang::En),
            "5s ago"
        );
        assert_eq!(
            relative_time(now - Duration::seconds(59), now, Lang::En),
            "59s ago"
        );
        assert_eq!(
            relative_time(now - Duration::seconds(60), now, Lang::En),
            "1m ago"
        );
        assert_eq!(
            relative_time(now - Duration::minutes(59), now, Lang::En),
            "59m ago"
        );
        assert_eq!(
            relative_time(now - Duration::minutes(60), now, Lang::En),
            "1h ago"
        );
        assert_eq!(
            relative_time(now - Duration::hours(23), now, Lang::En),
            "23h ago"
        );
        assert_eq!(
            relative_time(now - Duration::hours(24), now, Lang::En),
            "1d ago"
        );
        assert_eq!(
            relative_time(now - Duration::days(30), now, Lang::En),
            "30d ago"
        );
        // Clock skew falls back to a literal string rather than printing
        // a negative number.
        assert_eq!(
            relative_time(now + Duration::seconds(10), now, Lang::En),
            "in the future"
        );
    }

    #[test]
    fn relative_time_buckets_in_every_language() {
        let now = Utc::now();
        // Every language renders the same buckets; only the wording
        // differs. The English wording is pinned by
        // `relative_time_buckets`, the Chinese one by the table below.
        //
        // The lookup is an exhaustive `match` on purpose: a new
        // `Lang` variant does not compile until it comes with its
        // wording, so a third language cannot slip past this test.
        let wording = |lang: Lang| -> (&'static str, &'static str, &'static str) {
            match lang {
                Lang::En => ("5s ago", "1d ago", "in the future"),
                Lang::ZhCn => ("5 秒前", "1 天前", "时间在未来"),
            }
        };
        for lang in Lang::ALL {
            let (seconds, days, future) = wording(lang);
            assert_eq!(
                relative_time(now - Duration::seconds(5), now, lang),
                seconds
            );
            assert_eq!(relative_time(now - Duration::hours(24), now, lang), days);
            assert_eq!(
                relative_time(now + Duration::seconds(10), now, lang),
                future
            );
        }
    }
}
