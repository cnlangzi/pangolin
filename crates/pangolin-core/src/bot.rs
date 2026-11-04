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
}
