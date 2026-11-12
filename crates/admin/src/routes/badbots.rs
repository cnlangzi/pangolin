//! IP reputation configuration (issue #141).
//!
//! The reputation check is configured in the **database**, not in
//! `ngx.yml`: the only credential it needs is a provider API key, and
//! a key in the configuration file ends up in backups, in a config
//! management system and in a shell history. The dashboard edits the
//! `badbots_config` singleton row and the gateway rebuilds the
//! library from it.
//!
//! The key fields are write-only. The form shows whether a key is
//! configured and takes a new one to replace it; the stored key is
//! never rendered back into the page, and an empty field keeps the
//! stored key instead of clearing it. Clearing a key is an explicit
//! action — its own checkbox.

use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;
use pangolin_core::{App, BadbotsEvidencePolicy, BadbotsSettings};

use super::helpers::{flash_error, param_bool, redirect};

/// The smallest refresh interval the gateway accepts, in seconds.
///
/// A zero interval would make the refresh loop spin at full speed and
/// hammer the publishers without pause.
const MIN_REFRESH_INTERVAL_SECS: u64 = 1;

/// Parse the form-encoded body of the reputation form.
///
/// `current` is the stored row: a key field that is empty keeps the
/// stored key, and only the matching "clear" checkbox removes it.
pub fn parse_badbots_form(
    body: &[u8],
    current: &BadbotsSettings,
) -> Result<BadbotsSettings, String> {
    let policy_raw = super::helpers::require_param(body, "badbots_evidence_policy")
        .unwrap_or_else(|| current.evidence_policy.as_str().to_string());
    let evidence_policy = BadbotsEvidencePolicy::parse(&policy_raw).ok_or_else(|| {
        format!("Invalid evidence policy '{policy_raw}'. Expected one of: strict, lenient.")
    })?;

    let interval_raw = super::helpers::require_param(body, "badbots_feed_refresh_interval_secs")
        .unwrap_or_else(|| current.feed_refresh_interval_secs.to_string());
    let interval: u64 = interval_raw.trim().parse().map_err(|_| {
        format!("Invalid refresh interval '{interval_raw}': expected a whole number of seconds.")
    })?;
    if interval < MIN_REFRESH_INTERVAL_SECS {
        return Err(format!(
            "The refresh interval must be at least {MIN_REFRESH_INTERVAL_SECS} second."
        ));
    }

    Ok(BadbotsSettings {
        enabled: param_bool(body, "badbots_enabled", current.enabled),
        evidence_policy,
        feed_refresh_interval_secs: interval,
        abuseipdb_api_key: api_key(
            body,
            "badbots_abuseipdb_api_key",
            "badbots_abuseipdb_clear",
            current.abuseipdb_api_key.clone(),
        ),
        crowdsec_api_key: api_key(
            body,
            "badbots_crowdsec_api_key",
            "badbots_crowdsec_clear",
            current.crowdsec_api_key.clone(),
        ),
        updated_at: chrono::Utc::now(),
    })
}

/// The new value of a key field.
///
/// Three rules, in this order:
///
/// * the "clear" checkbox is ticked → the key is removed;
/// * the field carries a value → the key is replaced by it;
/// * the field is empty → the stored key is kept.
///
/// The stored key is never rendered back into the form, so an empty
/// field is the normal case after a save — it must not silently
/// delete the operator's credential.
fn api_key(body: &[u8], field: &str, clear_field: &str, current: Option<String>) -> Option<String> {
    if param_bool(body, clear_field, false) {
        return None;
    }
    match super::helpers::require_param(body, field) {
        Some(value) => Some(value.trim().to_string()).filter(|v| !v.is_empty()),
        None => current,
    }
}

/// Persist the settings and rebuild the reputation library.
///
/// The library is rebuilt because a provider key is baked into the
/// instance when it is built: a new key needs a new instance, there is
/// no way to change one in place. The rebuild happens **after** the
/// row is written, so the new instance always carries the stored key.
pub async fn save_badbots_settings(app: &Arc<App>, cfg: BadbotsSettings) -> Result<(), String> {
    {
        let db = app.db.lock().await;
        pangolin_core::db::update_badbots_config(&db, &cfg)
            .map_err(|e| format!("Database error: {e}"))?;
    }
    app.rebuild_badbots().await;
    Ok(())
}

/// `POST /settings/badbots` — validate, persist, rebuild, redirect.
///
/// A failure re-renders the settings page with the error message
/// inline, so the operator can fix the input without losing the rest
/// of the form.
pub async fn handle_update_badbots(
    app: &Arc<App>,
    body: &[u8],
) -> http::Result<Response<Full<Bytes>>> {
    let current = match app.badbots_settings().await {
        Ok(c) => c,
        Err(e) => return flash_error(&format!("Reading the current settings failed: {e}")),
    };
    let parsed = match parse_badbots_form(body, &current) {
        Ok(p) => p,
        Err(msg) => return flash_error(&msg),
    };
    if let Err(msg) = save_badbots_settings(app, parsed).await {
        return flash_error(&msg);
    }
    Ok(redirect("/settings"))
}

/// `GET /api/badbots/config` — the current settings as JSON.
///
/// The keys are **not** part of the response: only whether a key is
/// configured.
pub async fn handle_get_badbots_config(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    let settings = match app.badbots_settings().await {
        Ok(s) => s,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(
                    serde_json::json!({ "error": format!("reading the settings failed: {e}") })
                        .to_string(),
                )))
                .map_err(|e: http::Error| {
                    log::error!("badbots: building the error response failed: {e}");
                    e
                });
        }
    };
    let body = serde_json::json!({
        "enabled": settings.enabled,
        "evidence_policy": settings.evidence_policy.as_str(),
        "feed_refresh_interval_secs": settings.feed_refresh_interval_secs,
        "abuseipdb_api_key_configured": settings.abuseipdb_api_key.is_some(),
        "crowdsec_api_key_configured": settings.crowdsec_api_key.is_some(),
        "updated_at": settings.updated_at.to_rfc3339(),
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> BadbotsSettings {
        BadbotsSettings {
            enabled: true,
            evidence_policy: BadbotsEvidencePolicy::Lenient,
            feed_refresh_interval_secs: 300,
            abuseipdb_api_key: Some("stored-key".into()),
            crowdsec_api_key: None,
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn an_empty_key_field_keeps_the_stored_key() {
        // The stored key is never rendered back into the form, so an
        // empty field is the normal case after a save — it must not
        // delete the operator's credential.
        let parsed = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=lenient&badbots_feed_refresh_interval_secs=300",
            &current(),
        )
        .expect("the form parses");
        assert_eq!(parsed.abuseipdb_api_key.as_deref(), Some("stored-key"));
    }

    #[test]
    fn a_typed_key_replaces_the_stored_one() {
        let parsed = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=lenient&badbots_feed_refresh_interval_secs=300&badbots_abuseipdb_api_key=new-key",
            &current(),
        )
        .expect("the form parses");
        assert_eq!(parsed.abuseipdb_api_key.as_deref(), Some("new-key"));
    }

    #[test]
    fn the_clear_checkbox_removes_the_stored_key() {
        let parsed = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=lenient&badbots_feed_refresh_interval_secs=300&badbots_abuseipdb_clear=true",
            &current(),
        )
        .expect("the form parses");
        assert_eq!(parsed.abuseipdb_api_key, None);
    }

    #[test]
    fn the_checkbox_can_turn_the_check_off() {
        // An unchecked checkbox is not submitted at all, so the form
        // pairs it with a hidden "false" field.
        let parsed = parse_badbots_form(
            b"badbots_enabled=false&badbots_evidence_policy=lenient&badbots_feed_refresh_interval_secs=300",
            &current(),
        )
        .expect("the form parses");
        assert!(!parsed.enabled);
    }

    #[test]
    fn an_unknown_policy_is_rejected() {
        let err = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=bogus&badbots_feed_refresh_interval_secs=300",
            &current(),
        )
        .expect_err("the policy must be validated");
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn a_zero_interval_is_rejected() {
        // A zero interval would make the refresh loop spin at full
        // speed and hammer the publishers without pause.
        let err = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=lenient&badbots_feed_refresh_interval_secs=0",
            &current(),
        )
        .expect_err("the interval must be validated");
        assert!(err.contains("at least"), "{err}");
    }

    #[test]
    fn the_policy_round_trips_through_the_database_spelling() {
        assert_eq!(
            BadbotsEvidencePolicy::parse("lenient"),
            Some(BadbotsEvidencePolicy::Lenient)
        );
        assert_eq!(
            BadbotsEvidencePolicy::parse("STRICT"),
            Some(BadbotsEvidencePolicy::Strict)
        );
        assert_eq!(BadbotsEvidencePolicy::parse("bogus"), None);
    }
}
