//! Bot identity types + thin adapter over [`knownbots`].
//!
//! The previous substring UA table (`BOT_RULES` / `detect_bot(ua)`) fully
//! trusted the client-supplied User-Agent. That let forged UAs pollute
//! `/logs/bots`. Verification now requires UA **and** IP ownership
//! (official CIDR or cached RDNS). Only [`knownbots::VerifyStatus::Verified`]
//! produces a [`BotIdentity`]; pending / failed / unknown are treated as
//! "not a bot" for the side-channel (fail closed).

use std::net::IpAddr;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use knownbots::{Validator, VerifyResult, VerifyStatus};

/// Identity of a verified bot, ready for the JSONL / SSE / stats sinks.
///
/// `name` is the registry id (YAML `name`, e.g. `"googlebot"`).
/// It is stored as JSONL `bot_name` so readers do not scan the
/// raw User-Agent again. Strings are owned because they come from
/// the YAML-loaded knownbots registry rather than a `'static` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotIdentity {
    /// Registry id written to JSONL `bot_name`, e.g. `"googlebot"`.
    pub name: String,
    /// Owning organisation, e.g. `"Google"`.
    pub vendor: String,
    /// Coarse bucket for filtering / colouring in the UI.
    pub category: BotCategory,
}

/// Coarse classification of a bot. SEO dashboards group SearchEngine
/// vs AI vs Social etc. so operators can answer "how much of our
/// crawl budget is being consumed by AI scrapers?" at a glance.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotCategory {
    /// Traditional web search crawlers (Googlebot, Baiduspider, …).
    SearchEngine,
    /// AI model trainers / inference scrapers (GPTBot, ClaudeBot, …).
    AiBot,
    /// Link-preview unfurlers for messaging / social platforms
    /// (facebookexternalhit, TelegramBot, …).
    Social,
    /// Site-monitoring / uptime probes (UptimeRobot, Pingdom, …).
    Monitoring,
    /// Ad-quality crawlers (AdsBot-Google, Mediapartners-Google, …).
    AdsBot,
}

impl std::fmt::Display for BotCategory {
    /// Human-readable label matching the serde snake_case wire
    /// format (`search_engine`, `ai_bot`, `social`, `monitoring`,
    /// `ads_bot`). The admin UI's `/logs/bots` page uses this
    /// via Askama's `{{ row.bot_category }}` interpolation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BotCategory::SearchEngine => "search_engine",
            BotCategory::AiBot => "ai_bot",
            BotCategory::Social => "social",
            BotCategory::Monitoring => "monitoring",
            BotCategory::AdsBot => "ads_bot",
        };
        f.write_str(s)
    }
}

impl BotCategory {
    /// The Tailwind class token of the category badge.
    ///
    /// The admin UI colours a bot's category the same way everywhere:
    /// the badge on the bot-log rows, the badge on the site's
    /// bot-limited list. One mapping, so the two never drift.
    pub fn class(&self) -> &'static str {
        match self {
            BotCategory::SearchEngine => {
                "bg-blue-100 dark:bg-blue-900/30 text-blue-800 dark:text-blue-200"
            }
            BotCategory::AiBot => {
                "bg-purple-100 dark:bg-purple-900/30 text-purple-800 dark:text-purple-200"
            }
            BotCategory::Social => {
                "bg-pink-100 dark:bg-pink-900/30 text-pink-800 dark:text-pink-200"
            }
            BotCategory::Monitoring => {
                "bg-amber-100 dark:bg-amber-900/30 text-amber-800 dark:text-amber-200"
            }
            BotCategory::AdsBot => {
                "bg-orange-100 dark:bg-orange-900/30 text-orange-800 dark:text-orange-200"
            }
        }
    }

    /// Parse the wire label produced by knownbots `BotKind::to_category_label`.
    pub fn from_label(label: &str) -> Self {
        match label {
            "search_engine" => BotCategory::SearchEngine,
            "ai_bot" => BotCategory::AiBot,
            "social" => BotCategory::Social,
            "ads_bot" => BotCategory::AdsBot,
            _ => BotCategory::Monitoring,
        }
    }
}

impl BotIdentity {
    /// Build from a knownbots verified result.
    pub fn from_verified(r: &VerifyResult) -> Self {
        Self {
            name: r.name.clone(),
            vendor: r.vendor.clone(),
            category: BotCategory::from_label(r.category),
        }
    }
}

/// Verify `ua` + `ip` against the knownbots registry.
///
/// Returns `Some(BotIdentity)` **only** when status is `Verified`.
/// Pending (async RDNS in flight), Failed, and Unknown all return
/// `None` — the caller must not write a bot-log entry.
pub fn verify_bot(validator: &Validator, ua: &str, ip: IpAddr) -> Option<BotIdentity> {
    let result = validator.verify(ua, ip);
    if result.status == VerifyStatus::Verified {
        Some(BotIdentity::from_verified(&result))
    } else {
        None
    }
}

/// One entry of the knownbots registry, as the admin UI shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotInfo {
    /// Registry id — the same string the JSONL bot log writes as
    /// `bot_name` and the same string the `P-Known-Bot` header
    /// carries.
    pub name: String,
    /// Owning organisation, e.g. `"Google"`.
    pub vendor: String,
    /// Coarse bucket for filtering / colouring in the UI.
    pub category: BotCategory,
}

/// The embedded registry, parsed once.
///
/// The YAML is compiled into the binary, so the result is a constant:
/// parsing it per call would cost 45 YAML documents and 45+ CIDR
/// parses for nothing.
static EMBEDDED_REGISTRY: OnceLock<Vec<BotInfo>> = OnceLock::new();

/// Every bot of the knownbots registry.
///
/// `verifier` is the **runtime** registry: the bots the verifier
/// knows, which include the YAML an operator dropped into
/// `<cache root>/conf.d/`. `None` falls back to the embedded
/// registry — what a deployment without a verifier sees.
pub fn registry(verifier: Option<&Validator>) -> Vec<BotInfo> {
    match verifier {
        Some(v) => v.bots().iter().map(bot_info).collect(),
        None => EMBEDDED_REGISTRY
            .get_or_init(|| {
                knownbots::load_bots(None)
                    .map(|bots| bots.iter().map(bot_info).collect())
                    .unwrap_or_default()
            })
            .clone(),
    }
}

/// Map one registry entry to its admin-facing shape.
fn bot_info(bot: &knownbots::Bot) -> BotInfo {
    BotInfo {
        name: bot.name.clone(),
        vendor: bot.vendor.clone(),
        category: BotCategory::from_label(bot.kind.to_category_label(&bot.name)),
    }
}

/// The state of one bot's IP list.
///
/// Plain data so the admin crate can render it without depending on
/// `knownbots`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BotVerificationState {
    /// The bot can be verified against a loaded address space.
    Ready,
    /// URL-backed bot whose IP list has never been downloaded.
    Cold,
    /// The last refresh failed.
    Failed,
}

impl std::fmt::Display for BotVerificationState {
    /// Lower-case wire label (`ready`, `cold`, `failed`) — the same
    /// shape as [`BotCategory`]'s labels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BotVerificationState::Ready => "ready",
            BotVerificationState::Cold => "cold",
            BotVerificationState::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// Why a bot's IP list could not be refreshed.
///
/// The split matters: `Unreachable` is an *environment* signal (the
/// vendor's endpoint refused us, the network was down) while
/// `Broken` points at this codebase (the list did not parse, our own
/// TLS handshake failed). It decides whether the operator waits or
/// files a bug.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BotRefreshErrorKind {
    /// The vendor's endpoint could not be reached or refused the
    /// request.
    Unreachable,
    /// The list arrived but is unusable, or our own client failed.
    Broken,
}

impl BotRefreshErrorKind {
    /// The wire label (`unreachable` / `broken`).
    pub fn as_str(self) -> &'static str {
        match self {
            BotRefreshErrorKind::Unreachable => "unreachable",
            BotRefreshErrorKind::Broken => "broken",
        }
    }
}

impl std::fmt::Display for BotRefreshErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Health of one bot's IP list, as plain data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotVerificationStatus {
    /// Registry id (e.g. `googlebot`).
    pub name: String,
    /// Owning organisation (e.g. `Google`).
    pub vendor: String,
    /// Coarse bucket for filtering / colouring in the UI.
    pub category: BotCategory,
    /// The derived state.
    pub state: BotVerificationState,
    /// Prefixes currently loaded (downloaded + custom).
    pub prefixes: usize,
    /// The failure kind, when `state == Failed`.
    pub error_kind: Option<BotRefreshErrorKind>,
    /// The failure message, when `state == Failed`.
    pub error_message: Option<String>,
}

impl BotVerificationStatus {
    /// The failure kind as a word (`unreachable` / `broken`), or an
    /// empty string when the bot is not failed.
    pub fn error_kind_label(&self) -> &'static str {
        match self.error_kind {
            Some(kind) => kind.as_str(),
            None => "",
        }
    }

    /// The failure message, or an empty string when there is none.
    pub fn error_message_text(&self) -> &str {
        self.error_message.as_deref().unwrap_or("")
    }

    /// The panel row's detail cell.
    ///
    /// A failed bot whose prefix set is still loaded is **not**
    /// fail-closed — the cached list keeps verifying — so the row
    /// says how many prefixes are still in use instead of implying
    /// the bot stopped verifying.
    pub fn detail_text(&self) -> String {
        match self.state {
            BotVerificationState::Failed => {
                let cached = match self.prefixes {
                    0 => String::new(),
                    1 => "1 prefix still loaded".to_string(),
                    n => format!("{n} prefixes still loaded"),
                };
                let msg = self.error_message_text();
                match (msg.is_empty(), cached.is_empty()) {
                    (true, true) => String::new(),
                    (true, false) => cached,
                    (false, true) => msg.to_string(),
                    (false, false) => format!("{msg} ({cached})"),
                }
            }
            BotVerificationState::Cold => "IP list never downloaded".to_string(),
            BotVerificationState::Ready => String::new(),
        }
    }
}

/// A bot config file that was skipped at load time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotConfigError {
    /// Path of the offending file.
    pub path: String,
    /// Why the file was skipped.
    pub message: String,
}

/// Health of the knownbots verification layer.
///
/// `crates/admin` does not depend on `knownbots`, so the state
/// crosses that boundary as these plain types.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BotVerificationHealth {
    /// One entry per registry bot.
    pub bots: Vec<BotVerificationStatus>,
    /// Operator config files that were skipped at load time.
    pub config_errors: Vec<BotConfigError>,
    /// Reverse-DNS lookups dropped because the work queue was full.
    pub rdns_overflow: u64,
    /// `false` when no verifier is attached — the panel then says
    /// the status is unavailable instead of claiming health.
    pub available: bool,
}

impl BotVerificationHealth {
    /// Bots whose address space is loaded and usable.
    pub fn ready_count(&self) -> usize {
        self.count(BotVerificationState::Ready)
    }

    /// URL-backed bots whose IP list has never been downloaded.
    pub fn cold_count(&self) -> usize {
        self.count(BotVerificationState::Cold)
    }

    /// Bots whose last refresh failed.
    pub fn failed_count(&self) -> usize {
        self.count(BotVerificationState::Failed)
    }

    /// The bots that need the operator's attention — everything that
    /// is not `Ready`.
    pub fn problem_bots(&self) -> Vec<&BotVerificationStatus> {
        self.bots
            .iter()
            .filter(|b| b.state != BotVerificationState::Ready)
            .collect()
    }

    /// True when something needs the operator's attention.
    pub fn needs_attention(&self) -> bool {
        !self.problem_bots().is_empty() || !self.config_errors.is_empty()
    }

    fn count(&self, state: BotVerificationState) -> usize {
        self.bots.iter().filter(|b| b.state == state).count()
    }
}

/// The health of the knownbots verification layer.
///
/// `None` (no verifier attached) yields an empty — and
/// *unavailable* — health value, never a panic and never a
/// "everything is fine" claim.
pub fn verification_health(verifier: Option<&Validator>) -> BotVerificationHealth {
    let Some(v) = verifier else {
        return BotVerificationHealth::default();
    };

    let bots = v
        .refresh_status()
        .into_iter()
        .map(|s| {
            let category = BotCategory::from_label(s.kind.to_category_label(&s.name));
            let (error_kind, error_message) = match &s.error {
                Some(knownbots::RefreshFailure::Unreachable(m)) => {
                    (Some(BotRefreshErrorKind::Unreachable), Some(m.clone()))
                }
                Some(knownbots::RefreshFailure::Broken(m)) => {
                    (Some(BotRefreshErrorKind::Broken), Some(m.clone()))
                }
                None => (None, None),
            };
            BotVerificationStatus {
                name: s.name,
                vendor: s.vendor,
                category,
                state: match s.state {
                    knownbots::BotRefreshState::Ready => BotVerificationState::Ready,
                    knownbots::BotRefreshState::Cold => BotVerificationState::Cold,
                    knownbots::BotRefreshState::Failed => BotVerificationState::Failed,
                },
                prefixes: s.prefixes,
                error_kind,
                error_message,
            }
        })
        .collect();

    BotVerificationHealth {
        bots,
        config_errors: v
            .config_errors()
            .into_iter()
            .map(|e| BotConfigError {
                path: e.path,
                message: e.message,
            })
            .collect(),
        rdns_overflow: v.rdns_overflow(),
        available: true,
    }
}

/// Whether `id` is safe to use as a form field name.
///
/// The per-site bot list is edited as one checkbox per bot, and the
/// field name is `rate_limit_bot_<id>`. An id carrying anything but
/// `A-Z`, `a-z`, `0-9`, `-` and `_` would produce a field name the
/// browser would not submit back under the same key, so such a bot
/// is not offered as a checkbox.
pub fn is_safe_bot_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn verified_googlebot_with_seeded_cidr() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("googlebot", "66.249.64.0/19").unwrap();
        let ua = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
        let ip = IpAddr::V4(Ipv4Addr::new(66, 249, 66, 1));
        let ident = verify_bot(&v, ua, ip).expect("verified");
        assert_eq!(ident.name, "googlebot");
        assert_eq!(ident.vendor, "Google");
        assert_eq!(ident.category, BotCategory::SearchEngine);
    }

    #[test]
    fn forged_ip_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("googlebot", "66.249.64.0/19").unwrap();
        let ua = "Mozilla/5.0 (compatible; Googlebot/2.1)";
        let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        assert!(verify_bot(&v, ua, ip).is_none());
    }

    #[test]
    fn pending_rdns_returns_none() {
        // Fail closed: cold RDNS must not enter the bot log.
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let ua = "Mozilla/5.0 (compatible; Baiduspider/2.0)";
        let ip = IpAddr::V4(Ipv4Addr::new(220, 181, 108, 94));
        assert!(verify_bot(&v, ua, ip).is_none());
    }

    #[test]
    fn wrong_case_ua_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("googlebot", "66.249.64.0/19").unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(66, 249, 66, 1));
        assert!(verify_bot(&v, "GOOGLEBOT/2.1", ip).is_none());
    }

    #[test]
    fn facebook_custom_cidr_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let ua = "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)";
        let ip = IpAddr::V4(Ipv4Addr::new(31, 13, 24, 10));
        let ident = verify_bot(&v, ua, ip).expect("verified");
        assert_eq!(ident.category, BotCategory::Social);
    }

    #[test]
    fn bot_category_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&BotCategory::SearchEngine).unwrap(),
            "\"search_engine\""
        );
        assert_eq!(
            serde_json::to_string(&BotCategory::AiBot).unwrap(),
            "\"ai_bot\""
        );
        assert_eq!(
            serde_json::to_string(&BotCategory::AdsBot).unwrap(),
            "\"ads_bot\""
        );
    }

    /// The registry the admin UI offers as checkboxes — the embedded
    /// bots when there is no verifier.
    #[test]
    fn registry_falls_back_to_the_embedded_bots() {
        let bots = registry(None);
        assert!(bots.iter().any(|b| b.name == "googlebot"));
        assert!(bots.iter().any(|b| b.name == "gptbot"));
        let google = bots.iter().find(|b| b.name == "googlebot").unwrap();
        assert_eq!(google.vendor, "Google");
        assert_eq!(google.category, BotCategory::SearchEngine);
    }

    /// The runtime registry — the bots the verifier knows, including
    /// the YAML an operator dropped into the cache root.
    #[test]
    fn registry_follows_the_verifier() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        std::fs::write(
            conf.join("custombot.yaml"),
            "name: custombot\nua: \"CustomBot\"\nvendor: \"Example\"\nkind: AITraining\n",
        )
        .unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let bots = registry(Some(&v));
        assert!(bots.iter().any(|b| b.name == "custombot"));
        assert!(
            !bots.iter().any(|b| b.name == "not-in-the-registry"),
            "only the bots the verifier knows are offered"
        );
    }

    /// Only ids that are safe as a form field name are offered as a
    /// checkbox — the field name is `rate_limit_bot_<id>`.
    #[test]
    fn safe_bot_ids_are_the_form_field_safe_ones() {
        assert!(is_safe_bot_id("googlebot"));
        assert!(is_safe_bot_id("applebot-extended"));
        assert!(is_safe_bot_id("chatgpt_user"));
        assert!(is_safe_bot_id("bot2"));
        assert!(!is_safe_bot_id(""));
        assert!(!is_safe_bot_id("bot name"));
        assert!(!is_safe_bot_id("bot.name"));
        assert!(!is_safe_bot_id("bot/name"));
        assert!(!is_safe_bot_id("bot\"onmouseover"));
    }

    /// Every category has a badge class — the admin UI colours the
    /// category the same way on the bot log and on the site's
    /// bot-limited list.
    #[test]
    fn every_category_has_a_badge_class() {
        for cat in [
            BotCategory::SearchEngine,
            BotCategory::AiBot,
            BotCategory::Social,
            BotCategory::Monitoring,
            BotCategory::AdsBot,
        ] {
            assert!(!cat.class().is_empty(), "category {cat:?} → empty class");
        }
    }

    #[test]
    fn category_from_label_roundtrip() {
        for label in ["search_engine", "ai_bot", "social", "monitoring", "ads_bot"] {
            assert_eq!(BotCategory::from_label(label).to_string(), label);
        }
    }

    /// No validator → an empty, *unavailable* health value. Not a
    /// panic, and not a "everything is fine" claim.
    #[test]
    fn verification_health_without_a_validator_is_empty() {
        let health = verification_health(None);
        assert!(health.bots.is_empty());
        assert!(health.config_errors.is_empty());
        assert_eq!(health.rdns_overflow, 0);
        assert!(!health.available);
        assert!(!health.needs_attention());
    }

    /// The health view maps the validator's state onto plain data:
    /// a URL-backed bot with no list is `cold`, an RDNS bot is
    /// `ready`, and a recorded refresh failure is `failed` with the
    /// failure kind.
    #[test]
    fn verification_health_maps_the_validator_state() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();

        let health = verification_health(Some(&v));
        assert!(health.available);
        let google = health.bots.iter().find(|b| b.name == "googlebot").unwrap();
        assert_eq!(google.state, BotVerificationState::Cold);
        assert_eq!(google.vendor, "Google");
        assert_eq!(google.category, BotCategory::SearchEngine);
        assert_eq!(google.error_kind, None);
        // An RDNS bot has no list to download — it is ready.
        let baidu = health
            .bots
            .iter()
            .find(|b| b.name == "baiduspider")
            .unwrap();
        assert_eq!(baidu.state, BotVerificationState::Ready);
        // Every URL-backed bot is cold until its list arrives.
        assert_eq!(health.cold_count(), health.problem_bots().len());
        assert!(health.cold_count() >= 1);
        assert_eq!(health.failed_count(), 0);
        assert!(health.ready_count() >= 1);

        // A seeded refresh failure becomes `failed` + a kind.
        v.seed_refresh_error(
            "googlebot",
            knownbots::RefreshFailure::Unreachable("vendor refused us".into()),
        )
        .unwrap();
        let health = verification_health(Some(&v));
        let google = health.bots.iter().find(|b| b.name == "googlebot").unwrap();
        assert_eq!(google.state, BotVerificationState::Failed);
        assert_eq!(google.error_kind, Some(BotRefreshErrorKind::Unreachable));
        assert_eq!(google.error_message.as_deref(), Some("vendor refused us"));
        assert_eq!(health.failed_count(), 1);
        assert!(health.problem_bots().iter().any(|b| b.name == "googlebot"));
        assert!(health.needs_attention());
    }

    /// A bot whose refresh failed but whose prefix set is still
    /// loaded keeps verifying from the cached list — it is *not*
    /// fail-closed. The panel must say so instead of implying the
    /// bot stopped verifying.
    #[test]
    fn a_failed_bot_with_a_loaded_prefix_set_still_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("googlebot", "66.249.64.0/19").unwrap();
        v.seed_refresh_error(
            "googlebot",
            knownbots::RefreshFailure::Unreachable("vendor refused us".into()),
        )
        .unwrap();

        // The bot still verifies from the cached prefixes …
        let ua = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
        let ip = IpAddr::V4(Ipv4Addr::new(66, 249, 66, 1));
        assert!(verify_bot(&v, ua, ip).is_some(), "still verifies");

        // … and the panel says the cached prefixes are still in use.
        let health = verification_health(Some(&v));
        let google = health.bots.iter().find(|b| b.name == "googlebot").unwrap();
        assert_eq!(google.state, BotVerificationState::Failed);
        assert_eq!(google.prefixes, 1);
        let detail = google.detail_text();
        assert!(
            detail.contains("vendor refused us"),
            "the failure message must survive: {detail}"
        );
        assert!(
            detail.contains("1 prefix still loaded"),
            "the row must say the cached prefixes are still in use: {detail}"
        );
    }

    /// A failed bot with **no** loaded prefixes is fail-closed — the
    /// row must not claim a cached list is still there.
    #[test]
    fn a_failed_bot_without_a_loaded_prefix_set_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_refresh_error(
            "googlebot",
            knownbots::RefreshFailure::Broken("list did not parse".into()),
        )
        .unwrap();

        let health = verification_health(Some(&v));
        let google = health.bots.iter().find(|b| b.name == "googlebot").unwrap();
        assert_eq!(google.state, BotVerificationState::Failed);
        assert_eq!(google.prefixes, 0);
        let detail = google.detail_text();
        assert!(detail.contains("list did not parse"), "{detail}");
        assert!(
            !detail.contains("prefixes still loaded"),
            "no cached list to report: {detail}"
        );
    }

    /// A malformed operator file reaches the health view as a
    /// config error — path and reason — so the admin panel can show
    /// it.
    #[test]
    fn verification_health_carries_config_errors() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        let broken = conf.join("broken.yaml");
        std::fs::write(&broken, "name: brokenbot\n  ua: [unclosed\n").unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();

        let health = verification_health(Some(&v));
        assert_eq!(health.config_errors.len(), 1);
        assert_eq!(health.config_errors[0].path, broken.display().to_string());
        assert!(!health.config_errors[0].message.is_empty());
        assert!(health.needs_attention());
    }
}
