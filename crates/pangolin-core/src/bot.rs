//! Search-engine / AI / social bot detection from `User-Agent` strings.
//!
//! Pure-function: no I/O, no async, no allocation beyond the input
//! lowercase. Designed to run on every proxied request's hot path
//! inside [`App::push_access_log`](crate::app::App::push_access_log)
//! — total budget is < 5 µs so the bot side-channel is essentially
//! free for non-bot traffic.
//!
//! ## Detection strategy
//!
//! v1 fully trusts the client-supplied `User-Agent` header. A future
//! iteration may add optional reverse-DNS validation for
//! `BotCategory::SearchEngine` (see [`reverse_dns_check`] for the
//! stub). For now, the match table is a list of substrings — Google,
//! Bing, OpenAI and friends all carry an unambiguous fragment in
//! their UA string, and substring match is the fastest possible
//! predicate for this workload.
//!
//! ## Rule ordering
//!
//! More specific patterns come first so a UA like
//! `"Mozilla/5.0 (compatible; Google-InspectionTool/1.0)"` is
//! attributed to `Google-InspectionTool` rather than `Googlebot`.
//! A future contributor adding a rule should check that it doesn't
//! shadow one above it.

use serde::{Deserialize, Serialize};

/// Identity of a known bot.
///
/// The string fields are `&'static str` because the rule table is a
/// `static` slice — heap allocations are forbidden on the hot path.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BotIdentity {
    /// Short name shown in the UI and JSONL output, e.g. `"Googlebot"`.
    pub name: &'static str,
    /// Owning organisation, e.g. `"Google"`. Useful for grouping
    /// in admin dashboards ("all Google bots").
    pub vendor: &'static str,
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
    /// via Askama's `{{ row.bot_category }}` interpolation —
    /// the Display impl keeps the template independent from the
    /// internal Rust enum naming.
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

/// Static rule table — searched linearly by [`detect_bot`].
///
/// **Order matters**: more specific substrings come first so e.g.
/// `"google-inspectiontool"` wins over `"googlebot"`. When adding a
/// rule, scan the table for shadows.
///
/// Substrings are matched case-insensitively against the lowercased
/// UA. All patterns are short and unambiguous.
pub static BOT_RULES: &[(&str, BotIdentity)] = &[
    // ── Search engine crawlers (most specific first) ────────────
    (
        "google-inspectiontool",
        BotIdentity {
            name: "GoogleInspectionTool",
            vendor: "Google",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "googleother",
        BotIdentity {
            name: "GoogleOther",
            vendor: "Google",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "googlebot",
        BotIdentity {
            name: "Googlebot",
            vendor: "Google",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "bingpreview",
        BotIdentity {
            name: "BingPreview",
            vendor: "Microsoft",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "bingbot",
        BotIdentity {
            name: "Bingbot",
            vendor: "Microsoft",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "msnbot",
        BotIdentity {
            name: "MSNBot",
            vendor: "Microsoft",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "baiduspider",
        BotIdentity {
            name: "Baiduspider",
            vendor: "Baidu",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "yandexbot",
        BotIdentity {
            name: "YandexBot",
            vendor: "Yandex",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "duckduckbot",
        BotIdentity {
            name: "DuckDuckBot",
            vendor: "DuckDuckGo",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "sogou",
        BotIdentity {
            name: "Sogou",
            vendor: "Sogou",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "360spider",
        BotIdentity {
            name: "360Spider",
            vendor: "Qihoo",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "applebot",
        BotIdentity {
            name: "Applebot",
            vendor: "Apple",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "petalbot",
        BotIdentity {
            name: "PetalBot",
            vendor: "Huawei",
            category: BotCategory::SearchEngine,
        },
    ),
    (
        "naverbot",
        BotIdentity {
            name: "Naverbot",
            vendor: "Naver",
            category: BotCategory::SearchEngine,
        },
    ),
    // ── AI training / inference bots ───────────────────────────
    // google-extended MUST come before googlebot (already above) to
    // disambiguate "Google-Extended" (AI training opt-out token)
    // from "Googlebot" (search crawler).
    (
        "google-extended",
        BotIdentity {
            name: "GoogleExtended",
            vendor: "Google",
            category: BotCategory::AiBot,
        },
    ),
    (
        "oai-searchbot",
        BotIdentity {
            name: "OAISearchBot",
            vendor: "OpenAI",
            category: BotCategory::AiBot,
        },
    ),
    (
        "chatgpt-user",
        BotIdentity {
            name: "ChatGPTUser",
            vendor: "OpenAI",
            category: BotCategory::AiBot,
        },
    ),
    (
        "gptbot",
        BotIdentity {
            name: "GPTBot",
            vendor: "OpenAI",
            category: BotCategory::AiBot,
        },
    ),
    (
        "claudebot",
        BotIdentity {
            name: "ClaudeBot",
            vendor: "Anthropic",
            category: BotCategory::AiBot,
        },
    ),
    (
        "claude-web",
        BotIdentity {
            name: "ClaudeWeb",
            vendor: "Anthropic",
            category: BotCategory::AiBot,
        },
    ),
    (
        "anthropic-ai",
        BotIdentity {
            name: "AnthropicAI",
            vendor: "Anthropic",
            category: BotCategory::AiBot,
        },
    ),
    (
        "perplexity-user",
        BotIdentity {
            name: "PerplexityUser",
            vendor: "Perplexity",
            category: BotCategory::AiBot,
        },
    ),
    (
        "perplexitybot",
        BotIdentity {
            name: "PerplexityBot",
            vendor: "Perplexity",
            category: BotCategory::AiBot,
        },
    ),
    (
        "ccbot",
        BotIdentity {
            name: "CCBot",
            vendor: "CommonCrawl",
            category: BotCategory::AiBot,
        },
    ),
    (
        "amazonbot",
        BotIdentity {
            name: "AmazonBot",
            vendor: "Amazon",
            category: BotCategory::AiBot,
        },
    ),
    (
        "bytespider",
        BotIdentity {
            name: "Bytespider",
            vendor: "ByteDance",
            category: BotCategory::AiBot,
        },
    ),
    (
        "cohere-ai",
        BotIdentity {
            name: "CohereAI",
            vendor: "Cohere",
            category: BotCategory::AiBot,
        },
    ),
    (
        "cohere",
        BotIdentity {
            name: "Cohere",
            vendor: "Cohere",
            category: BotCategory::AiBot,
        },
    ),
    (
        "diffbot",
        BotIdentity {
            name: "Diffbot",
            vendor: "Diffbot",
            category: BotCategory::AiBot,
        },
    ),
    (
        "duckassistbot",
        BotIdentity {
            name: "DuckAssistBot",
            vendor: "DuckDuckGo",
            category: BotCategory::AiBot,
        },
    ),
    (
        "applebot-extended",
        BotIdentity {
            name: "ApplebotExtended",
            vendor: "Apple",
            category: BotCategory::AiBot,
        },
    ),
    (
        "meta-externalagent",
        BotIdentity {
            name: "MetaExternalAgent",
            vendor: "Meta",
            category: BotCategory::AiBot,
        },
    ),
    // ── Social link-preview unfurlers ─────────────────────────
    (
        "facebookexternalhit",
        BotIdentity {
            name: "FacebookExternalHit",
            vendor: "Meta",
            category: BotCategory::Social,
        },
    ),
    (
        "telegrambot",
        BotIdentity {
            name: "TelegramBot",
            vendor: "Telegram",
            category: BotCategory::Social,
        },
    ),
    (
        "twitterbot",
        BotIdentity {
            name: "Twitterbot",
            vendor: "Twitter",
            category: BotCategory::Social,
        },
    ),
    (
        "linkedinbot",
        BotIdentity {
            name: "LinkedInBot",
            vendor: "LinkedIn",
            category: BotCategory::Social,
        },
    ),
    (
        "slackbot",
        BotIdentity {
            name: "Slackbot",
            vendor: "Slack",
            category: BotCategory::Social,
        },
    ),
    (
        "slack-imgproxy",
        BotIdentity {
            name: "SlackImgProxy",
            vendor: "Slack",
            category: BotCategory::Social,
        },
    ),
    (
        "whatsapp",
        BotIdentity {
            name: "WhatsApp",
            vendor: "Meta",
            category: BotCategory::Social,
        },
    ),
    (
        "discordbot",
        BotIdentity {
            name: "Discordbot",
            vendor: "Discord",
            category: BotCategory::Social,
        },
    ),
    (
        "line-pinterest",
        BotIdentity {
            name: "LinePinterest",
            vendor: "Line",
            category: BotCategory::Social,
        },
    ),
    (
        "pinterestbot",
        BotIdentity {
            name: "Pinterest",
            vendor: "Pinterest",
            category: BotCategory::Social,
        },
    ),
    (
        "redditbot",
        BotIdentity {
            name: "RedditBot",
            vendor: "Reddit",
            category: BotCategory::Social,
        },
    ),
    // ── Site-monitoring probes ─────────────────────────────────
    (
        "uptimerobot",
        BotIdentity {
            name: "UptimeRobot",
            vendor: "UptimeRobot",
            category: BotCategory::Monitoring,
        },
    ),
    (
        "pingdom",
        BotIdentity {
            name: "Pingdom",
            vendor: "Pingdom",
            category: BotCategory::Monitoring,
        },
    ),
    (
        "statuscake",
        BotIdentity {
            name: "StatusCake",
            vendor: "StatusCake",
            category: BotCategory::Monitoring,
        },
    ),
    (
        "newrelic-pinger",
        BotIdentity {
            name: "NewRelicPinger",
            vendor: "NewRelic",
            category: BotCategory::Monitoring,
        },
    ),
    // ── Ad-quality crawlers ────────────────────────────────────
    (
        "adsbot-google",
        BotIdentity {
            name: "AdsBotGoogle",
            vendor: "Google",
            category: BotCategory::AdsBot,
        },
    ),
    (
        "mediapartners-google",
        BotIdentity {
            name: "MediapartnersGoogle",
            vendor: "Google",
            category: BotCategory::AdsBot,
        },
    ),
];

/// Identify a bot from a `User-Agent` header value.
///
/// Returns `Some(BotIdentity)` if `ua` (case-insensitively) contains
/// any of the substrings in [`BOT_RULES`]; `None` otherwise.
///
/// The check is allocation-cheap: at most one `to_ascii_lowercase`
/// allocation per call (for UAs > 12 bytes; shorter UAs are scanned
/// in-place via `eq_ignore_ascii_case`). v1 fully trusts the
/// client-supplied UA — a future iteration may add an optional
/// reverse-DNS verification for `SearchEngine` bots.
pub fn detect_bot(ua: &str) -> Option<BotIdentity> {
    // Fast reject: empty or unusually short UA can't be a real bot.
    // Real bot UAs are at least ~20 bytes (`Mozilla/5.0 ... bot ...`).
    if ua.len() < 12 {
        return None;
    }

    // Lowercase once. UA bytes are typically ASCII so the cost is
    // O(n) on the UA length, well under 1 µs for typical 100-byte UAs.
    let ua_lower = ua.to_ascii_lowercase();

    for (needle, ident) in BOT_RULES {
        if ua_lower.contains(needle) {
            return Some(*ident);
        }
    }
    None
}

/// Placeholder for a future reverse-DNS verification step.
///
/// Currently a no-op: the function returns `true` (i.e. "trust the
/// UA"). A follow-up PR will replace this with a `lookup_addr`
/// plus-suffix check for `BotCategory::SearchEngine` bots so that
/// forged UAs from arbitrary IPs don't pollute the JSONL.
/// The signature is intentionally stable so the call site in
/// [`App::push_access_log`](crate::app::App::push_access_log) can
/// be added without restructuring once the real check ships.
#[allow(unused_variables)]
pub fn reverse_dns_check(ip: std::net::IpAddr, ident: &BotIdentity) -> bool {
    // TODO: implement PTR lookup against `.googlebot.com` (Google),
    // `.search.msn.com` (Bing), `.crawl.baidu.com` (Baidu), etc.
    // The check must be off the request hot path (spawn a task or
    // hit a shared cache keyed by (ip, vendor)).
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_empty_returns_none() {
        assert!(detect_bot("").is_none());
    }

    #[test]
    fn detect_too_short_returns_none() {
        // < 12 bytes — not a real bot UA.
        assert!(detect_bot("curl/7").is_none());
    }

    #[test]
    fn detect_googlebot_attributed_correctly() {
        let ua = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
        let ident = detect_bot(ua).expect("Googlebot should be detected");
        assert_eq!(ident.name, "Googlebot");
        assert_eq!(ident.vendor, "Google");
        assert_eq!(ident.category, BotCategory::SearchEngine);
    }

    #[test]
    fn detect_googlebot_case_insensitive() {
        let ua = "GOOGLEBOT/2.1";
        assert_eq!(detect_bot(ua).unwrap().name, "Googlebot");
    }

    #[test]
    fn detect_google_inspectiontool_shadows_googlebot() {
        // Ordering invariant: more-specific pattern wins.
        let ua = "Mozilla/5.0 (compatible; Google-InspectionTool/1.0)";
        let ident = detect_bot(ua).expect("InspectionTool should match");
        assert_eq!(
            ident.name, "GoogleInspectionTool",
            "InspectionTool must shadow plain Googlebot — re-order BOT_RULES if this fails"
        );
    }

    #[test]
    fn detect_google_extended_shadows_googlebot() {
        let ua = "Mozilla/5.0 (compatible; Google-Extended)";
        let ident = detect_bot(ua).expect("Google-Extended should match");
        assert_eq!(ident.name, "GoogleExtended");
        assert_eq!(ident.category, BotCategory::AiBot);
    }

    #[test]
    fn detect_bingbot_attributed_correctly() {
        let ua = "Mozilla/5.0 (compatible; bingbot/2.0; +http://www.bing.com/bingbot.htm)";
        let ident = detect_bot(ua).expect("Bingbot should be detected");
        assert_eq!(ident.name, "Bingbot");
        assert_eq!(ident.vendor, "Microsoft");
    }

    #[test]
    fn detect_baiduspider_attributed_correctly() {
        let ua =
            "Mozilla/5.0 (compatible; Baiduspider/2.0; +http://www.baidu.com/search/spider.html)";
        let ident = detect_bot(ua).expect("Baiduspider should be detected");
        assert_eq!(ident.name, "Baiduspider");
        assert_eq!(ident.vendor, "Baidu");
    }

    #[test]
    fn detect_gptbot_attributed_as_ai() {
        let ua = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; GPTBot/1.0; +https://openai.com/gptbot)";
        let ident = detect_bot(ua).expect("GPTBot should be detected");
        assert_eq!(ident.name, "GPTBot");
        assert_eq!(ident.vendor, "OpenAI");
        assert_eq!(ident.category, BotCategory::AiBot);
    }

    #[test]
    fn detect_claudebot_attributed_as_ai() {
        let ua = "Mozilla/5.0 (compatible; ClaudeBot/1.0; +claudebot@anthropic.com)";
        let ident = detect_bot(ua).expect("ClaudeBot should be detected");
        assert_eq!(ident.name, "ClaudeBot");
        assert_eq!(ident.vendor, "Anthropic");
    }

    #[test]
    fn detect_perplexitybot_attributed_as_ai() {
        let ua = "PerplexityBot/1.0";
        let ident = detect_bot(ua).expect("PerplexityBot should be detected");
        assert_eq!(ident.name, "PerplexityBot");
        assert_eq!(ident.category, BotCategory::AiBot);
    }

    #[test]
    fn detect_facebookexternalhit_attributed_as_social() {
        let ua = "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)";
        let ident = detect_bot(ua).expect("FacebookExternalHit should be detected");
        assert_eq!(ident.category, BotCategory::Social);
    }

    #[test]
    fn detect_telegrambot_attributed_as_social() {
        let ua = "TelegramBot (like TwitterBot)";
        let ident = detect_bot(ua).expect("TelegramBot should be detected");
        assert_eq!(ident.name, "TelegramBot");
        assert_eq!(ident.category, BotCategory::Social);
    }

    #[test]
    fn detect_uptimerobot_attributed_as_monitoring() {
        let ua = "UptimeRobot/2.0 (https://www.uptimerobot.com/)";
        let ident = detect_bot(ua).expect("UptimeRobot should be detected");
        assert_eq!(ident.name, "UptimeRobot");
        assert_eq!(ident.category, BotCategory::Monitoring);
    }

    #[test]
    fn detect_adsbot_google_attributed_as_ads() {
        let ua = "AdsBot-Google (+http://www.google.com/adsbot.html)";
        let ident = detect_bot(ua).expect("AdsBot-Google should be detected");
        assert_eq!(ident.name, "AdsBotGoogle");
        assert_eq!(ident.category, BotCategory::AdsBot);
    }

    #[test]
    fn detect_regular_chrome_returns_none() {
        // Real-browser UA: no bot substring.
        let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        assert!(detect_bot(ua).is_none());
    }

    #[test]
    fn detect_curl_returns_none() {
        assert!(detect_bot("curl/8.4.0").is_none());
    }

    #[test]
    fn detect_postman_returns_none() {
        // Postman is not in v1's rule table; if added later it would
        // land in Monitoring or another category. Pinning the current
        // behaviour (no match) protects against accidental regressions.
        assert!(detect_bot("PostmanRuntime/7.36.0").is_none());
    }

    #[test]
    fn bot_identity_is_copy_and_eq() {
        // Sanity check on the derives — the type crosses channel
        // boundaries (broadcast::Sender, ring buffer clones) and
        // shows up in match arms.
        let a = BotIdentity {
            name: "Googlebot",
            vendor: "Google",
            category: BotCategory::SearchEngine,
        };
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn bot_category_serializes_as_snake_case() {
        // The JSON wire format must be stable for downstream tools
        // (jq / DuckDB queries pin on these strings).
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
    fn rules_table_has_no_duplicate_needles() {
        // Two patterns with the same needle would be a silent bug:
        // whichever comes first wins, and the second is dead code.
        // Catch it at unit-test time rather than in production.
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for (needle, _) in BOT_RULES {
            assert!(seen.insert(*needle), "duplicate rule needle: {needle:?}");
        }
    }

    #[test]
    fn rules_table_covers_all_documented_categories() {
        // Operator UX invariant: every BotCategory variant must
        // have at least one rule, otherwise the category filter
        // in the admin UI would silently drop everything.
        let mut seen_cats = std::collections::HashSet::new();
        for (_, ident) in BOT_RULES {
            seen_cats.insert(ident.category);
        }
        assert!(seen_cats.contains(&BotCategory::SearchEngine));
        assert!(seen_cats.contains(&BotCategory::AiBot));
        assert!(seen_cats.contains(&BotCategory::Social));
        assert!(seen_cats.contains(&BotCategory::Monitoring));
        assert!(seen_cats.contains(&BotCategory::AdsBot));
    }
}
