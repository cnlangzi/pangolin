//! Provider configuration — the shared admin surface of the provider
//! registry (issue #122).
//!
//! Two things live here:
//!
//! * the **status** of every provider's published range list (the
//!   settings page's table) and of the origin-guard rejection
//!   counters;
//! * the **parsing** of the site form's provider fields — the
//!   provider select, the custom header names, the guard tri-state
//!   and the manual allow ranges.
//!
//! The registry itself lives in `pangolin_core::provider`; this module
//! only reads it. That is what keeps "add the next front CDN" a
//! one-place change: the admin UI builds its dropdown from
//! [`ProviderKey::all`] and never hard-codes a provider name.

use std::collections::HashMap;
use std::sync::Arc;

use pangolin_core::cidr::CidrSet;
use pangolin_core::provider::{GlobalProviderConfig, ProviderKey, SiteProviderOverride, spec};

use crate::App;

/// How many refresh intervals a range list may be old before the
/// admin UI marks it stale.
///
/// The list stays in force while it is stale — the guard never fails
/// open on staleness, because a false 403 window (a provider adds a
/// range) is worse than a stale list.
pub const STALE_INTERVALS: u32 = 3;

/// The status of one provider's published range list.
#[derive(Debug, Clone)]
pub struct ProviderRangeStatus {
    /// The provider.
    pub key: ProviderKey,
    /// The display label of the provider.
    pub label: &'static str,
    /// The number of ranges currently in force (the in-memory set).
    pub range_count: usize,
    /// When the list was last fetched, RFC-3339.
    pub fetched_at: Option<String>,
    /// `ok` after a successful fetch, `error` after a failed one.
    /// Empty when the list was never fetched.
    pub status: String,
    /// The error text of the last failed fetch.
    pub last_error: Option<String>,
    /// True when the list is older than [`STALE_INTERVALS`] refresh
    /// intervals.
    pub stale: bool,
    /// The number of origin-guard rejections seen for this provider.
    pub rejections: u64,
}

/// The status of every provider of the registry.
///
/// A provider that publishes no list (or whose list was never
/// fetched) is still listed — the operator has to be able to see that
/// the guard would have nothing to work with.
pub async fn provider_range_statuses(app: &Arc<App>) -> Vec<ProviderRangeStatus> {
    let stored = {
        let db = app.db.lock().await;
        pangolin_core::db::list_provider_ip_lists(&db).unwrap_or_default()
    };

    let stored: HashMap<(ProviderKey, String), pangolin_core::provider::ProviderIpList> = stored
        .into_iter()
        .map(|row| ((row.provider, row.url.clone()), row))
        .collect();

    let in_memory = app.provider_ranges.read();
    let now = chrono::Utc::now();
    let interval = app.config.provider.ip_refresh_interval_secs.max(1);
    let rejections = app.guard_rejections.snapshot();

    ProviderKey::all()
        .iter()
        .map(|key| {
            let key = *key;
            let range_count = in_memory.get(&key).map(|s| s.len()).unwrap_or(0);
            // The rows of one provider are merged for the display:
            // the status shown is the worst one (an error on any URL
            // means the list is incomplete).
            let mut status = String::new();
            let mut last_error: Option<String> = None;
            let mut fetched_at: Option<chrono::DateTime<chrono::Utc>> = None;
            let urls: Vec<String> = spec(key)
                .ranges
                .map(|r| r.urls.iter().map(|u| u.url.to_string()).collect())
                .unwrap_or_default();
            for url in &urls {
                let Some(row) = stored.get(&(key, url.clone())) else {
                    continue;
                };
                if row.status == pangolin_core::provider::STATUS_ERROR {
                    status = pangolin_core::provider::STATUS_ERROR.to_string();
                    last_error = row.last_error.clone().or(last_error);
                } else if status.is_empty() {
                    status = row.status.clone();
                }
                if fetched_at.is_none() || row.fetched_at > fetched_at.unwrap() {
                    fetched_at = Some(row.fetched_at);
                }
            }
            let stale = fetched_at
                .map(|t| (now - t).num_seconds() > (interval as i64) * STALE_INTERVALS as i64)
                .unwrap_or(false);
            ProviderRangeStatus {
                key,
                label: spec(key).label,
                range_count,
                fetched_at: fetched_at.map(|t| t.to_rfc3339()),
                status,
                last_error,
                stale,
                rejections: rejections.get(&key).copied().unwrap_or(0),
            }
        })
        .collect()
}

/// The total number of origin-guard rejections, across every
/// provider.
pub fn total_guard_rejections(app: &Arc<App>) -> u64 {
    app.guard_rejections.snapshot().values().sum()
}

/// The outcome of parsing the provider fields of the site form.
pub struct ParsedProvider {
    /// The per-site override. `None` → the site inherits every global
    /// default (no row is written).
    pub over: Option<SiteProviderOverride>,
    /// The manual allow ranges, in their canonical text form.
    pub manual_ranges: Vec<String>,
    /// The first error, if any field was malformed.
    pub error: Option<String>,
    /// False when the form carried no provider field at all — an API
    /// client that predates the feature must keep working, and an
    /// update must not silently reset the provider configuration the
    /// operator set through the UI.
    pub present: bool,
}

/// Parse the provider fields of the site form.
///
/// * `provider` — the provider select. An empty value means "inherit
///   the global default".
/// * `client_ip_header`, `country_header`, `region_header` — the
///   custom header names. An empty value means "inherit the global
///   name".
/// * `origin_guard` — the guard tri-state: `inherit` / `off` / `on`.
/// * `origin_allow` — the manual allow ranges, one CIDR per line.
///
/// A malformed CIDR is reported with a message naming the offending
/// value; nothing is stored when the parse fails.
pub fn parse_site_provider(params: &HashMap<String, String>) -> ParsedProvider {
    const FIELDS: [&str; 6] = [
        "provider",
        "client_ip_header",
        "country_header",
        "region_header",
        "origin_guard",
        "origin_allow",
    ];
    let present = FIELDS.iter().any(|f| params.contains_key(*f));

    let provider = match params.get("provider").map(|v| v.trim()) {
        None | Some("") => None,
        Some(raw) => match raw.parse::<ProviderKey>() {
            Ok(k) => Some(k),
            Err(_) => {
                return ParsedProvider {
                    over: None,
                    manual_ranges: Vec::new(),
                    error: Some(format!(
                        "Invalid provider '{raw}'. Expected one of: {}.",
                        expected_providers()
                    )),
                    present,
                };
            }
        },
    };

    let origin_guard = match params.get("origin_guard").map(|v| v.trim()) {
        None | Some("") | Some("inherit") => None,
        Some("off") | Some("false") | Some("0") => Some(false),
        Some("on") | Some("true") | Some("1") => Some(true),
        Some(other) => {
            return ParsedProvider {
                over: None,
                manual_ranges: Vec::new(),
                error: Some(format!(
                    "Invalid origin_guard '{other}'. Expected inherit, off or on."
                )),
                present,
            };
        }
    };

    let mut manual_ranges = Vec::new();
    let mut error: Option<String> = None;
    if let Some(raw) = params.get("origin_allow") {
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match pangolin_core::cidr::normalize_cidr(line) {
                Ok(normalized) => manual_ranges.push(normalized),
                Err(e) => {
                    if error.is_none() {
                        error = Some(format!("Invalid source range: {e}"));
                    }
                }
            }
        }
    }

    let client_ip_header = non_empty(params.get("client_ip_header"));
    let country_header = non_empty(params.get("country_header"));
    let region_header = non_empty(params.get("region_header"));

    // A site that overrides nothing at all inherits everything: no
    // row is written, so a later change of the global default is
    // picked up.
    let over = if provider.is_none()
        && origin_guard.is_none()
        && client_ip_header.is_none()
        && country_header.is_none()
        && region_header.is_none()
    {
        None
    } else {
        Some(SiteProviderOverride {
            provider,
            client_ip_header,
            country_header,
            region_header,
            origin_guard_enabled: origin_guard,
        })
    };

    ParsedProvider {
        over,
        manual_ranges,
        error,
        present,
    }
}

/// Check that the origin guard can actually be switched on.
///
/// A provider that publishes no range list (a customer-owned load
/// balancer) and a site with no manual ranges would have an empty
/// allowlist — the guard would then either reject every request or
/// fail open, depending on the implementation. The admin UI refuses
/// the submission instead, so the operator is told what to do.
///
/// A provider that *does* publish ranges is allowed even when the
/// list has not been fetched yet: the guard fails open until the
/// first successful fetch, and the operator can see the list's state
/// on the settings page.
pub fn guard_can_be_enabled(provider: ProviderKey, manual_ranges: &[String]) -> Result<(), String> {
    let publishes = spec(provider).ranges.is_some();
    if publishes || !manual_ranges.is_empty() {
        return Ok(());
    }
    Err(
        "The origin guard cannot be enabled: the provider publishes no source-IP ranges and the \
         site has no manual ranges. Add the load balancer's ranges first."
            .to_string(),
    )
}

/// The trimmed value of a form field, or `None` when it is absent or
/// blank.
fn non_empty(value: Option<&String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The comma-separated list of the registry's provider keys — used in
/// the "expected one of" error messages.
pub fn expected_providers() -> String {
    ProviderKey::all()
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The number of ranges currently in force for a provider.
pub fn provider_range_count(app: &Arc<App>, key: ProviderKey) -> usize {
    app.provider_ranges
        .read()
        .get(&key)
        .map(|s: &Arc<CidrSet>| s.len())
        .unwrap_or(0)
}

/// Every provider of the registry, as the site form's dropdown shows
/// them.
pub fn provider_options() -> Vec<crate::templates::ProviderOption> {
    ProviderKey::all()
        .iter()
        .map(|key| {
            let spec = spec(*key);
            crate::templates::ProviderOption {
                key: spec.key.as_str(),
                label: spec.label,
                description: spec.description,
            }
        })
        .collect()
}

/// The provider configuration of one site, as the site form shows it.
///
/// `site_name` is `None` for a site that does not exist yet — the
/// form then shows the global defaults.
pub async fn site_provider_view(
    app: &Arc<App>,
    site_name: Option<&str>,
) -> crate::templates::sites::SiteProviderView {
    let global_cfg = app.system_config.read().await.clone();
    let global = GlobalProviderConfig::from_system_config(&global_cfg);

    let (over, ranges) = match site_name {
        None => (SiteProviderOverride::default(), Vec::new()),
        Some(name) => {
            let db = app.db.lock().await;
            let over = pangolin_core::db::get_site_provider(&db, name)
                .ok()
                .flatten()
                .unwrap_or_default();
            let ranges = pangolin_core::db::list_site_origin_allow(&db)
                .ok()
                .unwrap_or_default()
                .remove(name)
                .unwrap_or_default();
            (over, ranges)
        }
    };

    let effective = global.resolve(&over);
    let (range_count, last_fetch) = provider_range_state(app, effective.provider).await;

    crate::templates::sites::SiteProviderView {
        provider: over
            .provider
            .map(|k| k.as_str().to_string())
            .unwrap_or_default(),
        global_provider: global.provider.as_str().to_string(),
        client_ip_header: over.client_ip_header.clone().unwrap_or_default(),
        country_header: over.country_header.clone().unwrap_or_default(),
        region_header: over.region_header.clone().unwrap_or_default(),
        origin_guard: match over.origin_guard_enabled {
            None => "inherit".to_string(),
            Some(false) => "off".to_string(),
            Some(true) => "on".to_string(),
        },
        origin_allow: ranges.join("\n"),
        effective_provider: effective.provider.as_str().to_string(),
        effective_range_count: range_count,
        effective_last_fetch: last_fetch.unwrap_or_default(),
    }
}

/// The state of one provider's published range list: the number of
/// ranges in force and when the list was last fetched.
///
/// The count comes from the in-memory set (what the guard actually
/// uses); the timestamp comes from the persisted row.
pub async fn provider_range_state(app: &Arc<App>, key: ProviderKey) -> (usize, Option<String>) {
    let count = provider_range_count(app, key);
    let db = app.db.lock().await;
    let rows = pangolin_core::db::list_provider_ip_lists(&db).unwrap_or_default();
    drop(db);
    let fetched_at = rows
        .iter()
        .filter(|r| r.provider == key)
        .map(|r| r.fetched_at)
        .max()
        .map(|t| t.to_rfc3339());
    (count, fetched_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_form_without_any_provider_field_is_ignored() {
        // An API client that predates the feature must keep working:
        // no field → the site's configuration is left alone.
        let params = form(&[("name", "site")]);
        let parsed = parse_site_provider(&params);
        assert!(!parsed.present);
        assert!(parsed.error.is_none());
        assert!(parsed.over.is_none());
    }

    #[test]
    fn the_provider_select_is_parsed() {
        let params = form(&[("provider", "cloudflare")]);
        let parsed = parse_site_provider(&params);
        assert!(parsed.present);
        assert!(parsed.error.is_none());
        let over = parsed.over.expect("an override");
        assert_eq!(over.provider, Some(ProviderKey::Cloudflare));
        assert_eq!(over.origin_guard_enabled, None);
    }

    #[test]
    fn an_empty_provider_means_inherit() {
        let params = form(&[("provider", "")]);
        let parsed = parse_site_provider(&params);
        assert!(parsed.error.is_none());
        // Nothing is overridden → no row is written.
        assert!(parsed.over.is_none());
    }

    #[test]
    fn an_unknown_provider_is_reported_with_the_registry_list() {
        let params = form(&[("provider", "fastly")]);
        let parsed = parse_site_provider(&params);
        let err = parsed.error.expect("an error");
        assert!(err.contains("fastly"), "{err}");
        assert!(err.contains("direct"), "{err}");
        assert!(err.contains("cloudflare"), "{err}");
    }

    #[test]
    fn the_guard_tri_state_is_parsed() {
        for (raw, expected) in [
            ("inherit", None),
            ("", None),
            ("off", Some(false)),
            ("on", Some(true)),
        ] {
            let params = form(&[("origin_guard", raw)]);
            let parsed = parse_site_provider(&params);
            assert!(parsed.error.is_none(), "{:?}", parsed.error);
            assert_eq!(
                parsed.over.and_then(|o| o.origin_guard_enabled),
                expected,
                "origin_guard={raw:?}"
            );
        }
    }

    #[test]
    fn the_manual_ranges_are_normalised_and_a_malformed_one_is_reported() {
        let params = form(&[("origin_allow", "203.0.113.0/24\n\n10.0.0.1\n")]);
        let parsed = parse_site_provider(&params);
        assert!(parsed.error.is_none(), "{:?}", parsed.error);
        assert_eq!(
            parsed.manual_ranges,
            vec!["203.0.113.0/24".to_string(), "10.0.0.1".to_string()]
        );

        let params = form(&[("origin_allow", "not-a-cidr")]);
        let parsed = parse_site_provider(&params);
        let err = parsed.error.expect("an error");
        assert!(err.contains("not-a-cidr"), "{err}");
    }

    #[test]
    fn the_guard_needs_an_allowlist() {
        // A provider that publishes no range list and a site with no
        // manual ranges → the guard would have nothing to work with.
        assert!(guard_can_be_enabled(ProviderKey::CustomLb, &[]).is_err());
        assert!(
            guard_can_be_enabled(ProviderKey::CustomLb, &["203.0.113.0/24".to_string()]).is_ok()
        );
        // A provider that publishes ranges is always allowed.
        assert!(guard_can_be_enabled(ProviderKey::Cloudflare, &[]).is_ok());
    }
}
