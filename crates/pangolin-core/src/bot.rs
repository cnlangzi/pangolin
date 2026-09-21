//! Bot identity types + thin adapter over [`knownbots`].
//!
//! The previous substring UA table (`BOT_RULES` / `detect_bot(ua)`) fully
//! trusted the client-supplied User-Agent. That let forged UAs pollute
//! `/logs/bots`. Verification now requires UA **and** IP ownership
//! (official CIDR or cached RDNS). Only [`knownbots::VerifyStatus::Verified`]
//! produces a [`BotIdentity`]; pending / failed / unknown are treated as
//! "not a bot" for the side-channel (fail closed).

use std::net::IpAddr;

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

    #[test]
    fn category_from_label_roundtrip() {
        for label in ["search_engine", "ai_bot", "social", "monitoring", "ads_bot"] {
            assert_eq!(BotCategory::from_label(label).to_string(), label);
        }
    }
}
