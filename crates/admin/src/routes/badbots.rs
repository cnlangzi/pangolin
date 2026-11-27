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

use super::helpers::{param_bool, redirect};
use crate::i18n::Lang;

/// Parse the form-encoded body of the reputation form.
///
/// `current` is the stored row: a key field that is empty keeps the
/// stored key, and only the matching "clear" checkbox removes it.
/// A field the form does not carry keeps its stored value as well —
/// this is the **merge** flavour, the one the admin form uses.
///
/// The two version counters are left at zero: they are the **write
/// layer's** business — `db::update_badbots_config` decides them and
/// ignores whatever the parser put here (issue #273).
pub fn parse_badbots_form(
    body: &[u8],
    current: &BadbotsSettings,
) -> Result<BadbotsSettings, String> {
    parse_badbots_form_with(body, current, current.evidence_policy, current.enabled)
}

/// Parse the form-encoded body of the reputation form the way the
/// **dashboard** writes it: the body is the desired state (issue
/// #273).
///
/// A field the body does not mention falls back to its default — the
/// check is on and the evidence bar is `lenient`. The credentials are
/// the exception: a key the body does not mention **keeps the stored
/// one**, because the dashboard only ever sees *whether* a key is
/// configured and could not restore a key it never read.
pub fn parse_badbots_form_replaced(
    body: &[u8],
    current: &BadbotsSettings,
) -> Result<BadbotsSettings, String> {
    parse_badbots_form_with(body, current, BadbotsEvidencePolicy::Lenient, true)
}

/// The shared body of the two flavours of the reputation form.
///
/// The two flavours differ in exactly two values — what an omitted
/// field falls back to — and in nothing else, so they share one
/// parser: the **merge** flavour passes the stored values, the
/// **replace** flavour the documented defaults.
///
/// `current` is the stored row. It is the fallback for the
/// credentials in both flavours: a key the body does not mention
/// keeps the stored one, because a credential is write-only.
fn parse_badbots_form_with(
    body: &[u8],
    current: &BadbotsSettings,
    policy_fallback: BadbotsEvidencePolicy,
    enabled_fallback: bool,
) -> Result<BadbotsSettings, String> {
    let policy_raw = super::helpers::require_param(body, "badbots_evidence_policy")
        .unwrap_or_else(|| policy_fallback.as_str().to_string());
    let evidence_policy = BadbotsEvidencePolicy::parse(&policy_raw).ok_or_else(|| {
        format!("Invalid evidence policy '{policy_raw}'. Expected one of: strict, lenient.")
    })?;

    Ok(BadbotsSettings {
        enabled: param_bool(body, "badbots_enabled", enabled_fallback),
        evidence_policy,
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
        revision: 0,
        synced_revision: 0,
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
/// delete the operator's credential. A field that carries nothing but
/// whitespace counts as empty: it is a stray space in a password
/// input, not an intent to remove the credential. Only the checkbox
/// removes a key.
fn api_key(body: &[u8], field: &str, clear_field: &str, current: Option<String>) -> Option<String> {
    if param_bool(body, clear_field, false) {
        return None;
    }
    match super::helpers::require_param(body, field) {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                current
            } else {
                Some(trimmed.to_string())
            }
        }
        None => current,
    }
}

/// Persist the settings and rebuild the reputation library.
///
/// The library is rebuilt because a provider key is baked into the
/// instance when it is built: a new key needs a new instance, there is
/// no way to change one in place. The rebuild happens **after** the
/// row is written, so the new instance always carries the stored key.
///
/// The write is a **local** one: the write layer leaves the sync
/// counter alone, so the row counts as drifted from what the
/// dashboard last pushed (issue #273).
pub async fn save_badbots_settings(app: &Arc<App>, cfg: BadbotsSettings) -> Result<(), String> {
    // The write runs on the blocking pool (issue #254).
    app.with_db_res(move |db| {
        pangolin_core::db::update_badbots_config(db, &cfg, pangolin_core::ConfigWriteSource::Admin)
    })
    .await
    .map_err(|e| format!("Database error: {e}"))?;
    app.rebuild_badbots().await;
    Ok(())
}

/// `POST /settings/badbots` — validate, persist, rebuild, redirect.
///
/// A failure re-renders the settings page with the error message
/// inline, where the operator was, instead of a standalone error
/// page. The page is rendered from the stored row, so the values that
/// were typed but not saved are lost.
pub async fn handle_update_badbots(
    app: &Arc<App>,
    csrf: &str,
    body: &[u8],
    lang: Lang,
) -> http::Result<Response<Full<Bytes>>> {
    let current = match app.badbots_settings().await {
        Ok(c) => c,
        Err(e) => {
            return super::settings::render_with_error(
                app,
                csrf,
                &format!("Reading the current settings failed: {e}"),
                lang,
            )
            .await;
        }
    };
    let parsed = match parse_badbots_form(body, &current) {
        Ok(p) => p,
        Err(msg) => return super::settings::render_with_error(app, csrf, &msg, lang).await,
    };
    if let Err(msg) = save_badbots_settings(app, parsed).await {
        return super::settings::render_with_error(app, csrf, &msg, lang).await;
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
            abuseipdb_api_key: Some("stored-key".into()),
            crowdsec_api_key: None,
            revision: 1,
            synced_revision: 0,
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn an_empty_key_field_keeps_the_stored_key() {
        // The stored key is never rendered back into the form, so an
        // empty field is the normal case after a save — it must not
        // delete the operator's credential.
        let parsed = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=lenient",
            &current(),
        )
        .expect("the form parses");
        assert_eq!(parsed.abuseipdb_api_key.as_deref(), Some("stored-key"));
    }

    /// The **replace** flavour: a field the body does not mention
    /// falls back to its default (issue #273, R2/R5).
    #[test]
    fn the_replace_parser_falls_back_to_the_defaults() {
        let parsed = parse_badbots_form_replaced(b"", &current()).expect("the body parses");
        assert!(
            parsed.enabled,
            "an omitted switch falls back to its documented default"
        );
        assert_eq!(
            parsed.evidence_policy,
            BadbotsEvidencePolicy::Lenient,
            "an omitted policy falls back to its documented default"
        );
    }

    /// The credentials are the exception: a key the body does not
    /// mention keeps the stored one (issue #273, R3).
    #[test]
    fn the_replace_parser_keeps_the_stored_credentials() {
        let parsed = parse_badbots_form_replaced(b"badbots_enabled=false", &current())
            .expect("the body parses");
        assert!(!parsed.enabled);
        assert_eq!(parsed.abuseipdb_api_key.as_deref(), Some("stored-key"));
        assert_eq!(parsed.crowdsec_api_key, None);
    }

    /// The clear flag removes the stored key, a value replaces it.
    #[test]
    fn the_replace_parser_clears_and_replaces_the_credentials() {
        let cleared = parse_badbots_form_replaced(b"badbots_abuseipdb_clear=true", &current())
            .expect("the body parses");
        assert_eq!(cleared.abuseipdb_api_key, None);

        let replaced =
            parse_badbots_form_replaced(b"badbots_abuseipdb_api_key=new-key", &current())
                .expect("the body parses");
        assert_eq!(replaced.abuseipdb_api_key.as_deref(), Some("new-key"));
    }

    #[test]
    fn a_typed_key_replaces_the_stored_one() {
        let parsed = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=lenient&badbots_abuseipdb_api_key=new-key",
            &current(),
        )
        .expect("the form parses");
        assert_eq!(parsed.abuseipdb_api_key.as_deref(), Some("new-key"));
    }

    #[test]
    fn the_clear_checkbox_removes_the_stored_key() {
        let parsed = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=lenient&badbots_abuseipdb_clear=true",
            &current(),
        )
        .expect("the form parses");
        assert_eq!(parsed.abuseipdb_api_key, None);
    }

    #[test]
    fn a_whitespace_only_key_field_keeps_the_stored_key() {
        // A stray space in the password input is not an intent to
        // remove the credential: only the checkbox removes a key.
        let parsed = parse_badbots_form(
            // `%20` is a percent-encoded space — `require_param` keeps a
            // literal `+` as `+`, so the space has to be encoded.
            b"badbots_enabled=true&badbots_evidence_policy=lenient&badbots_abuseipdb_api_key=%20",
            &current(),
        )
        .expect("the form parses");
        assert_eq!(parsed.abuseipdb_api_key.as_deref(), Some("stored-key"));
    }

    #[test]
    fn the_checkbox_can_turn_the_check_off() {
        // An unchecked checkbox is not submitted at all, so the form
        // pairs it with a hidden "false" field.
        let parsed = parse_badbots_form(
            b"badbots_enabled=false&badbots_evidence_policy=lenient",
            &current(),
        )
        .expect("the form parses");
        assert!(!parsed.enabled);
    }

    #[test]
    fn an_unknown_policy_is_rejected() {
        let err = parse_badbots_form(
            b"badbots_enabled=true&badbots_evidence_policy=bogus",
            &current(),
        )
        .expect_err("the policy must be validated");
        assert!(err.contains("bogus"), "{err}");
    }

    /// The list-refresh-interval setting is gone (issue #151).
    ///
    /// The knob decided only how often the gateway *asked* the library
    /// whether a list was due; the publisher's own refresh floor
    /// decides that, and the floor is 24 hours now. A setting that
    /// looks meaningful and is not is worse than no setting — so the
    /// API does not report one any more.
    #[tokio::test]
    async fn the_api_response_has_no_refresh_interval() {
        use http_body_util::BodyExt;

        let dir = tempfile::TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let app = Arc::new(
            App::new(
                db_path.to_str().expect("utf8"),
                pangolin_core::Config::default(),
                pangolin_core::CertManager::default(),
            )
            .expect("App::new"),
        );

        let response = handle_get_badbots_config(&app)
            .await
            .expect("the response builds");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("the body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert!(
            json.get("feed_refresh_interval_secs").is_none(),
            "the removed setting must not be in the response: {json}"
        );
        // The settings that are still there.
        assert_eq!(
            json.get("evidence_policy").and_then(|v| v.as_str()),
            Some("lenient")
        );
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
