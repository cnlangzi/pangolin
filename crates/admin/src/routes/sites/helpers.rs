//! Internal helpers for the `sites` resource.
//!
//! - `parse_form`         — decode `application/x-www-form-urlencoded`
//! - `assemble_url`       — `<scheme>://<host>` or `file:///<path>`
//! - `assemble_backend_from_form` — reconstruct the backend string from
//!   the individual visible form fields when the hidden `backend` field
//!   is empty on submit (JS didn't fire, etc.)
//! - `parse_site_form`    — validate the whole site form and return the
//!   parsed values (issue #185: the dashboard-facing JSON API shares
//!   this parser with the HTML form, so both reject the same inputs
//!   for the same reasons)
//!
//! These are kept in `mod.rs` (private) rather than in the `helpers`
//! module under `routes/`, which is for *cross-resource* helpers.

use std::collections::{BTreeSet, HashMap};

use pangolin_core::provider::ProviderKey;
use pangolin_core::ratelimit::RateLimitPolicy;
use pangolin_core::types::HostMode;

use crate::routes::provider::{ParsedProvider, guard_can_be_enabled, parse_site_provider};
use crate::routes::ratelimit::{
    CUSTOM_403_FIELD, CUSTOM_429_FIELD, parse_bot_limits, parse_policy,
};

/// Build `<scheme>://<host>` for http/https, or `file:///<path>` for file.
pub fn assemble_url(scheme: &str, host: &str) -> String {
    if scheme == "file" {
        let path = host.trim_start_matches('/');
        format!("file:///{}", path)
    } else {
        format!("{}://{}", scheme, host)
    }
}

/// Reconstruct the backend string from the individual visible form
/// fields (route_mode, direct_protocol, direct_host, tun_name,
/// tunnel_protocol, tunnel_host). Used as a fallback when the hidden
/// `backend` field is empty on submit (e.g. JS didn't update it).
///
/// Returns an empty string if any required piece is missing.
pub fn assemble_backend_from_form(params: &HashMap<String, String>) -> String {
    let route_mode = params
        .get("route_mode")
        .cloned()
        .unwrap_or_else(|| "direct".to_string());
    if route_mode == "tunnel" {
        let tun = params.get("tun_name").cloned().unwrap_or_default();
        let proto = params
            .get("tunnel_protocol")
            .cloned()
            .unwrap_or_else(|| "http".to_string());
        let host = params.get("tunnel_host").cloned().unwrap_or_default();
        if tun.is_empty() || host.trim().is_empty() {
            return String::new();
        }
        format!("{}:{}", tun, assemble_url(&proto, host.trim()))
    } else {
        let proto = params
            .get("direct_protocol")
            .cloned()
            .unwrap_or_else(|| "http".to_string());
        let host = params.get("direct_host").cloned().unwrap_or_default();
        if host.trim().is_empty() {
            return String::new();
        }
        assemble_url(&proto, host.trim())
    }
}

/// The backend the submission asks for.
///
/// The hidden `backend` field is filled in by the page's JS from the
/// visible form fields. If JS didn't fire (e.g. browser quirk, the
/// user pasted a value that didn't trigger an input event, or the
/// page was re-rendered mid-typing), the hidden can be empty even
/// though the user did fill in host:port. As a safety net, assemble
/// the backend server-side from the individual form fields when
/// `backend` is empty.
pub fn site_backend_from_form(params: &HashMap<String, String>) -> String {
    let backend_from_hidden = params.get("backend").cloned().unwrap_or_default();
    if backend_from_hidden.is_empty() {
        assemble_backend_from_form(params)
    } else {
        backend_from_hidden
    }
}

/// Rebuild the site a submission describes.
///
/// Used to re-render the form when a submission is rejected, so the
/// operator does not lose what they typed.
pub fn site_prefill(params: &HashMap<String, String>, name: &str) -> pangolin_core::types::Site {
    pangolin_core::types::Site {
        name: name.to_string(),
        backend: site_backend_from_form(params),
        host_mode: params
            .get("host_mode")
            .and_then(|v| v.parse::<HostMode>().ok())
            .unwrap_or(HostMode::Passthrough),
        host_custom: params.get("host_custom").cloned().filter(|v| !v.is_empty()),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        domain_count: 0,
    }
}

/// The parsed contents of a site form.
///
/// Everything the handler needs to write the site, its rate-limit
/// policy, its bot-limited list and its provider override.
pub struct SiteForm {
    pub name: String,
    pub backend: String,
    pub host_mode: HostMode,
    pub host_custom: Option<String>,
    /// The rate-limit policy, or `None` when the form carried no
    /// rate-limit field at all — "leave the stored policy alone".
    pub rate_limit: Option<RateLimitPolicy>,
    /// The bot-limited list, or `None` when the form carried no list
    /// at all — "leave the stored list alone".
    pub rate_limit_bots: Option<BTreeSet<String>>,
    /// The provider block: the override, the manual ranges and
    /// whether the form mentioned the provider at all.
    pub provider: ParsedProvider,
}

/// Which part of the site form was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteFormSection {
    /// The provider block (provider, header names, guard, ranges).
    Provider,
    /// The rate-limit block.
    RateLimit,
    /// The origin guard cannot be enabled.
    Guard,
    /// A plain field.
    Field,
}

/// Why a site form was rejected.
///
/// The section mirrors the blocks of the form, so the HTML handler can
/// point at the right one and the JSON API can answer with the same
/// message.
///
/// The error carries the values that were parsed before the
/// rejection, so the form can be re-rendered with the operator's
/// input intact. That makes it a big value — it is **boxed** in the
/// result type, because a rejection is the rare path and the
/// allocation does not matter there.
pub struct SiteFormError {
    /// Which part of the form was rejected.
    pub section: SiteFormSection,
    /// The message the operator sees.
    pub message: String,
    /// The form field the message belongs to, when there is one.
    pub field: Option<&'static str>,
    /// The rate-limit policy as parsed — the re-render shows it.
    pub rate_limit: RateLimitPolicy,
    /// The bot-limited list the submission carried, if any.
    pub rate_limit_bots: Option<BTreeSet<String>>,
    /// The provider block as parsed.
    pub provider: ParsedProvider,
}

/// Parse and validate a site form.
///
/// The same parser serves the HTML form and the dashboard-facing JSON
/// API (issue #185) — a submission the form rejects is rejected here
/// too, with the same message.
///
/// * `name` — the site's name: from the form on create, from the path
///   on update. An empty name is "Site name is required".
/// * `stored_policy` — the site's stored rate-limit policy, used to
///   keep a custom error page the submission did not mention. `None`
///   on create.
/// * `global_provider` — the provider of the global configuration. A
///   site that overrides nothing inherits it, and the origin guard
///   needs *some* allowlist to work with.
pub fn parse_site_form(
    params: &HashMap<String, String>,
    name: &str,
    stored_policy: Option<&RateLimitPolicy>,
    global_provider: ProviderKey,
) -> Result<SiteForm, Box<SiteFormError>> {
    let backend = site_backend_from_form(params);
    let host_mode = params
        .get("host_mode")
        .and_then(|v| v.parse::<HostMode>().ok())
        .unwrap_or(HostMode::Passthrough);
    let host_custom = params.get("host_custom").cloned().filter(|v| !v.is_empty());

    // Issue #122: the provider fields (provider, custom header names,
    // the guard tri-state and the manual ranges).
    let provider = parse_site_provider(params);

    // Issue #117: the per-site rate-limit policy. A form without any
    // rate-limit field (an API client that predates the feature)
    // leaves the site without a policy row — the default (limits off)
    // applies.
    let parsed = parse_policy(params);

    // Issue #131: the per-site list of known bots that are NOT exempt
    // from this site's rate limits. `None` means the submission did
    // not carry the list at all, so no list is written.
    let rate_limit_bots = parse_bot_limits(params);

    // The origin guard needs an allowlist to work with: the provider's
    // published ranges or the site's manual ones.
    let guard_error = if provider.present {
        let guard_on = provider
            .over
            .as_ref()
            .and_then(|o| o.origin_guard_enabled)
            .unwrap_or(false);
        if guard_on {
            // The **effective** provider decides whether the guard has
            // anything to work with: a site that inherits the global
            // `cloudflare` provider inherits its published ranges too.
            let effective = provider
                .over
                .as_ref()
                .and_then(|o| o.provider)
                .unwrap_or(global_provider);
            guard_can_be_enabled(effective, &provider.manual_ranges).err()
        } else {
            None
        }
    } else {
        None
    };

    // The checks run in the order the form presents them: the
    // provider block, then the rate limits, then the guard, then the
    // plain fields.
    let rejected = if let Some(e) = &provider.error {
        Some((SiteFormSection::Provider, e.clone(), Some("provider")))
    } else if let Some(e) = &parsed.error {
        Some((SiteFormSection::RateLimit, e.clone(), Some("rate_limit")))
    } else if let Some(e) = &guard_error {
        Some((SiteFormSection::Guard, e.clone(), Some("provider")))
    } else if name.is_empty() {
        Some((
            SiteFormSection::Field,
            "Site name is required".to_string(),
            Some("name"),
        ))
    } else if backend.is_empty() {
        Some((
            SiteFormSection::Field,
            "Backend is required — fill in the host:port (or file path) field".to_string(),
            Some("backend"),
        ))
    } else if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        Some((
            SiteFormSection::Field,
            format!("Invalid backend: {}", e),
            Some("backend"),
        ))
    } else {
        None
    };

    if let Some((section, message, field)) = rejected {
        return Err(Box::new(SiteFormError {
            section,
            message,
            field,
            rate_limit: parsed.policy,
            rate_limit_bots,
            provider,
        }));
    }

    let mut rate_limit = parsed.present.then_some(parsed.policy);
    // A form that carries the limit knobs but not the custom-page
    // textarea (an API client that only knows the numeric knobs) must
    // not wipe the stored page.
    if let Some(stored) = stored_policy
        && let Some(policy) = rate_limit.as_mut()
    {
        if !params.contains_key(CUSTOM_429_FIELD) {
            policy.custom_429_body = stored.custom_429_body.clone();
        }
        if !params.contains_key(CUSTOM_403_FIELD) {
            policy.custom_403_body = stored.custom_403_body.clone();
        }
    }

    Ok(SiteForm {
        name: name.to_string(),
        backend,
        host_mode,
        host_custom,
        rate_limit,
        rate_limit_bots,
        provider,
    })
}
