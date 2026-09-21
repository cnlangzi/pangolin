//! Bot configuration loaded from YAML (embedded + optional override dir).

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use ipnet::IpNet;
use parking_lot::RwLock;
use serde::Deserialize;

use crate::lru::FailLru;
use crate::rdns::RdnsCache;

/// Coarse bot category from the upstream YAML `kind` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Default)]
pub enum BotKind {
    SearchEngine,
    SocialMedia,
    AITraining,
    AIAssist,
    AIMixed,
    SEO,
    Monitor,
    Security,
    Scraper,
    #[default]
    #[serde(other)]
    Unknown,
}

impl BotKind {
    /// Map to pangolin's five-bucket `BotCategory` wire labels.
    ///
    /// Ads-related monitors (`adsbot*`, `mediapartners*`) land in
    /// `ads_bot` so the existing admin filter keeps working; other
    /// `Monitor` bots become `monitoring`.
    pub fn to_category_label(self, bot_name: &str) -> &'static str {
        match self {
            BotKind::SearchEngine | BotKind::SEO => "search_engine",
            BotKind::AITraining | BotKind::AIAssist | BotKind::AIMixed => "ai_bot",
            BotKind::SocialMedia => "social",
            BotKind::Monitor => {
                if bot_name.contains("ads") || bot_name.contains("mediapartners") {
                    "ads_bot"
                } else {
                    "monitoring"
                }
            }
            BotKind::Security | BotKind::Scraper | BotKind::Unknown => "monitoring",
        }
    }
}

#[derive(Debug, Deserialize)]
struct BotConfigFile {
    name: String,
    #[serde(default)]
    kind: BotKind,
    #[serde(default)]
    parser: String,
    #[serde(default)]
    ua: String,
    /// Owning organisation for `bot_vendor`. Empty → [`vendor_for`].
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    urls: Vec<String>,
    #[serde(default)]
    custom: Vec<String>,
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    rdns: bool,
}

/// Runtime bot definition. Prefixes / RDNS state live behind locks so
/// the background scheduler can swap them without blocking readers
/// for long.
pub struct Bot {
    pub name: String,
    pub kind: BotKind,
    pub parser: String,
    /// Case-sensitive UA marker (e.g. `"Googlebot"`, `"bingbot"`).
    /// Matching only — the JSONL `bot_name` is [`Self::name`].
    pub ua: String,
    /// Owning organisation written to JSONL `bot_vendor`.
    pub vendor: String,
    pub urls: Vec<String>,
    pub custom: Vec<IpNet>,
    pub domains: Vec<String>,
    pub rdns: bool,
    /// Merged custom + downloaded prefixes. Readers take a short
    /// `RwLock` read; writers replace the whole `Vec`.
    pub prefixes: RwLock<Vec<IpNet>>,
    pub rdns_cache: Option<Arc<RdnsCache>>,
    pub fail_cache: Option<Arc<FailLru>>,
}

impl Bot {
    fn from_config(cfg: BotConfigFile) -> Result<Self> {
        // Empty parser means "no structured format" — plain CIDR lines.
        // Substituting `cfg.name` used to look like a custom parser and
        // then silently hit the txt fallback inside `parser::parse`.
        let parser = if cfg.parser.is_empty() {
            "txt".to_string()
        } else {
            cfg.parser
        };
        if !cfg.urls.is_empty() && !crate::parser::is_known(&parser) {
            log::warn!(
                "bot {}: unknown parser {parser:?}; IP lists will be parsed as plain text",
                cfg.name
            );
        }
        let vendor = if cfg.vendor.is_empty() {
            let fallback = vendor_for(&cfg.name);
            if fallback == "Unknown" {
                log::warn!(
                    "bot {}: no vendor in config; JSONL bot_vendor will be \"Unknown\"",
                    cfg.name
                );
            }
            fallback.to_string()
        } else {
            cfg.vendor
        };
        let mut custom = Vec::new();
        for cidr in &cfg.custom {
            match cidr.parse::<IpNet>() {
                Ok(net) => custom.push(net),
                Err(e) => log::warn!("bot {}: skip bad custom CIDR {cidr:?}: {e}", cfg.name),
            }
        }
        Ok(Self {
            name: cfg.name,
            kind: cfg.kind,
            parser,
            ua: cfg.ua,
            vendor,
            urls: cfg.urls,
            custom: custom.clone(),
            domains: cfg.domains,
            rdns: cfg.rdns,
            prefixes: RwLock::new(custom),
            rdns_cache: None,
            fail_cache: None,
        })
    }

    /// True if `ip` is covered by the current prefix set.
    pub fn contains_ip(&self, ip: IpAddr) -> bool {
        let prefixes = self.prefixes.read();
        prefixes.iter().any(|net| net.contains(&ip))
    }

    /// Replace downloaded prefixes while keeping static `custom` ones.
    pub fn store_downloaded(&self, downloaded: Vec<IpNet>) {
        if downloaded.is_empty() {
            // Never wipe a working set on a failed refresh.
            return;
        }
        let mut merged = self.custom.clone();
        merged.extend(downloaded);
        *self.prefixes.write() = merged;
    }

    /// Load prefixes from a previously persisted `ips.txt`.
    pub fn load_cached_ips(&self, path: &Path) {
        let Ok(data) = std::fs::read_to_string(path) else {
            return;
        };
        let mut nets = Vec::new();
        for line in data.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Ok(net) = line.parse::<IpNet>() {
                nets.push(net);
            }
        }
        if !nets.is_empty() {
            self.store_downloaded(nets);
        }
    }

    /// Persist current prefixes (custom + downloaded) to `ips.txt`.
    pub fn persist_ips(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let prefixes = self.prefixes.read();
        let mut body = String::new();
        for net in prefixes.iter() {
            body.push_str(&net.to_string());
            body.push('\n');
        }
        std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

/// Embedded YAML shipped with the crate (search / AI / social /
/// monitoring bots only — HTTP-client fingerprints are omitted).
pub const EMBEDDED_CONFIGS: &[(&str, &str)] = &[
    (
        "adsbot-mobile.yaml",
        include_str!("../conf.d/adsbot-mobile.yaml"),
    ),
    ("adsbot.yaml", include_str!("../conf.d/adsbot.yaml")),
    ("ahrefsbot.yaml", include_str!("../conf.d/ahrefsbot.yaml")),
    ("amazonbot.yaml", include_str!("../conf.d/amazonbot.yaml")),
    (
        "apis-google.yaml",
        include_str!("../conf.d/apis-google.yaml"),
    ),
    (
        "applebot-extended.yaml",
        include_str!("../conf.d/applebot-extended.yaml"),
    ),
    ("applebot.yaml", include_str!("../conf.d/applebot.yaml")),
    (
        "baiduspider.yaml",
        include_str!("../conf.d/baiduspider.yaml"),
    ),
    ("bingbot.yaml", include_str!("../conf.d/bingbot.yaml")),
    ("bytespider.yaml", include_str!("../conf.d/bytespider.yaml")),
    ("ccbot.yaml", include_str!("../conf.d/ccbot.yaml")),
    (
        "chatgpt-user.yaml",
        include_str!("../conf.d/chatgpt-user.yaml"),
    ),
    ("claudebot.yaml", include_str!("../conf.d/claudebot.yaml")),
    (
        "cloudflare-alwaysonline.yaml",
        include_str!("../conf.d/cloudflare-alwaysonline.yaml"),
    ),
    (
        "cloudflare-healthchecks.yaml",
        include_str!("../conf.d/cloudflare-healthchecks.yaml"),
    ),
    ("discordbot.yaml", include_str!("../conf.d/discordbot.yaml")),
    (
        "duckduckbot.yaml",
        include_str!("../conf.d/duckduckbot.yaml"),
    ),
    (
        "facebookexternalhit.yaml",
        include_str!("../conf.d/facebookexternalhit.yaml"),
    ),
    (
        "feedfetcher.yaml",
        include_str!("../conf.d/feedfetcher.yaml"),
    ),
    (
        "google-extended.yaml",
        include_str!("../conf.d/google-extended.yaml"),
    ),
    (
        "google-inspectiontool.yaml",
        include_str!("../conf.d/google-inspectiontool.yaml"),
    ),
    (
        "google-storebot.yaml",
        include_str!("../conf.d/google-storebot.yaml"),
    ),
    ("googlebot.yaml", include_str!("../conf.d/googlebot.yaml")),
    (
        "googleother.yaml",
        include_str!("../conf.d/googleother.yaml"),
    ),
    ("gptbot.yaml", include_str!("../conf.d/gptbot.yaml")),
    (
        "linkedinbot.yaml",
        include_str!("../conf.d/linkedinbot.yaml"),
    ),
    (
        "mediapartners-google.yaml",
        include_str!("../conf.d/mediapartners-google.yaml"),
    ),
    (
        "meta-externalagent.yaml",
        include_str!("../conf.d/meta-externalagent.yaml"),
    ),
    ("mj12bot.yaml", include_str!("../conf.d/mj12bot.yaml")),
    (
        "oai-searchbot.yaml",
        include_str!("../conf.d/oai-searchbot.yaml"),
    ),
    (
        "perplexity-user.yaml",
        include_str!("../conf.d/perplexity-user.yaml"),
    ),
    (
        "perplexitybot.yaml",
        include_str!("../conf.d/perplexitybot.yaml"),
    ),
    ("petalbot.yaml", include_str!("../conf.d/petalbot.yaml")),
    ("pingdom.yaml", include_str!("../conf.d/pingdom.yaml")),
    (
        "pinterestbot.yaml",
        include_str!("../conf.d/pinterestbot.yaml"),
    ),
    ("redditbot.yaml", include_str!("../conf.d/redditbot.yaml")),
    (
        "semrushbot-backlinks.yaml",
        include_str!("../conf.d/semrushbot-backlinks.yaml"),
    ),
    ("semrushbot.yaml", include_str!("../conf.d/semrushbot.yaml")),
    ("slackbot.yaml", include_str!("../conf.d/slackbot.yaml")),
    ("sogou.yaml", include_str!("../conf.d/sogou.yaml")),
    (
        "telegrambot.yaml",
        include_str!("../conf.d/telegrambot.yaml"),
    ),
    ("twitterbot.yaml", include_str!("../conf.d/twitterbot.yaml")),
    (
        "uptimerobot.yaml",
        include_str!("../conf.d/uptimerobot.yaml"),
    ),
    ("whatsapp.yaml", include_str!("../conf.d/whatsapp.yaml")),
    ("yandexbot.yaml", include_str!("../conf.d/yandexbot.yaml")),
];

fn parse_yaml(data: &str, filename: &str) -> Result<Option<Bot>> {
    let cfg: BotConfigFile =
        serde_yaml::from_str(data).with_context(|| format!("parse bot config {filename}"))?;
    if cfg.name.is_empty() {
        log::warn!("skip {filename}: missing required 'name' field");
        return Ok(None);
    }
    if cfg.ua.is_empty() {
        log::warn!("skip {filename}: missing required 'ua' field");
        return Ok(None);
    }
    Ok(Some(Bot::from_config(cfg)?))
}

/// Load embedded configs, then overlay any `*.yaml` / `*.yml` from
/// `override_dir/conf.d` (same-name wins).
pub fn load_bots(override_dir: Option<&Path>) -> Result<Vec<Bot>> {
    use std::collections::HashMap;

    let mut by_name: HashMap<String, Bot> = HashMap::new();
    for (filename, data) in EMBEDDED_CONFIGS {
        if let Some(bot) = parse_yaml(data, filename)? {
            by_name.insert(bot.name.clone(), bot);
        }
    }

    if let Some(root) = override_dir {
        let conf_d = root.join("conf.d");
        if conf_d.is_dir() {
            for entry in
                std::fs::read_dir(&conf_d).with_context(|| format!("read {}", conf_d.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ext != "yaml" && ext != "yml" {
                    continue;
                }
                let data = std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?;
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("custom.yaml");
                if let Some(bot) = parse_yaml(&data, name)? {
                    log::info!("knownbots: custom config overrides bot {}", bot.name);
                    by_name.insert(bot.name.clone(), bot);
                }
            }
        }
    }

    let mut bots: Vec<Bot> = by_name.into_values().collect();
    // Stable order for tests / debugging.
    bots.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(bots)
}

/// URL-backed bots that still have an empty prefix set.
///
/// Fresh processes (no `ips.txt` yet) sit in this window until the
/// scheduler's first refresh. Claims against them fail closed.
pub(crate) fn count_cold_url_bots(bots: &[Bot]) -> usize {
    bots.iter()
        .filter(|b| !b.urls.is_empty() && b.prefixes.read().is_empty())
        .count()
}

/// Heuristic vendor label for the admin UI / JSONL `bot_vendor` field.
pub fn vendor_for(bot_name: &str) -> &'static str {
    match bot_name {
        n if n.starts_with("google")
            || n.starts_with("adsbot")
            || n.starts_with("apis-")
            || n.starts_with("mediapartners")
            || n.starts_with("feedfetcher") =>
        {
            "Google"
        }
        "bingbot" => "Microsoft",
        "baiduspider" => "Baidu",
        "yandexbot" => "Yandex",
        "duckduckbot" => "DuckDuckGo",
        "sogou" => "Sogou",
        "applebot" | "applebot-extended" => "Apple",
        "petalbot" => "Huawei",
        "gptbot" | "chatgpt-user" | "oai-searchbot" => "OpenAI",
        "claudebot" => "Anthropic",
        "amazonbot" => "Amazon",
        "meta-externalagent" | "facebookexternalhit" | "whatsapp" => "Meta",
        "perplexity-user" | "perplexitybot" => "Perplexity",
        "bytespider" => "ByteDance",
        "ccbot" => "CommonCrawl",
        "linkedinbot" => "LinkedIn",
        "twitterbot" => "Twitter",
        "pinterestbot" => "Pinterest",
        "telegrambot" => "Telegram",
        "slackbot" => "Slack",
        "discordbot" => "Discord",
        "redditbot" => "Reddit",
        "uptimerobot" => "UptimeRobot",
        "pingdom" => "Pingdom",
        n if n.starts_with("cloudflare") => "Cloudflare",
        n if n.starts_with("semrush") => "Semrush",
        "ahrefsbot" => "Ahrefs",
        "mj12bot" => "Majestic",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_embedded_has_googlebot() {
        let bots = load_bots(None).unwrap();
        assert!(bots.iter().any(|b| b.name == "googlebot"));
        assert!(bots.iter().any(|b| b.name == "claudebot"));
        assert!(bots.iter().any(|b| b.name == "applebot-extended"));
        assert!(bots.len() >= 40);
    }

    #[test]
    fn kind_maps_ads_to_ads_bot() {
        assert_eq!(BotKind::Monitor.to_category_label("adsbot"), "ads_bot");
        assert_eq!(
            BotKind::Monitor.to_category_label("uptimerobot"),
            "monitoring"
        );
    }

    #[test]
    fn embedded_bots_declare_vendor() {
        let bots = load_bots(None).unwrap();
        assert!(count_cold_url_bots(&bots) > 0);
        for b in &bots {
            assert_ne!(
                b.vendor, "Unknown",
                "bot {} has no vendor; add it to the YAML",
                b.name
            );
        }
    }

    /// JSONL `bot_name` is the registry id (`name`), not the UA
    /// marker and not a second spelling of it. The raw header is
    /// stored separately, so readers never re-scan it.
    #[test]
    fn registry_name_is_independent_of_ua_marker() {
        let bots = load_bots(None).unwrap();
        let bing = bots.iter().find(|b| b.name == "bingbot").unwrap();
        assert_eq!(bing.ua, "bingbot");
        let google = bots.iter().find(|b| b.name == "googlebot").unwrap();
        assert_eq!(google.ua, "Googlebot");
        let extended = bots.iter().find(|b| b.name == "applebot-extended").unwrap();
        assert_eq!(extended.ua, "Applebot-Extended");
    }

    #[test]
    fn empty_parser_defaults_to_txt_not_bot_name() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        std::fs::write(
            conf.join("custombot.yaml"),
            "name: custombot\nua: \"CustomBot\"\nvendor: \"Example\"\n",
        )
        .unwrap();
        let bots = load_bots(Some(dir.path())).unwrap();
        let bot = bots.iter().find(|b| b.name == "custombot").unwrap();
        assert_eq!(bot.parser, "txt");
        assert_eq!(bot.vendor, "Example");
        assert_eq!(bot.name, "custombot");
        assert_eq!(bot.ua, "CustomBot");
    }

    #[test]
    fn unknown_parser_name_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        std::fs::write(
            conf.join("typo.yaml"),
            "name: typobot\nua: \"TypoBot\"\nparser: googel\nurls:\n  - \"https://example.invalid/ips.json\"\n",
        )
        .unwrap();
        let bots = load_bots(Some(dir.path())).unwrap();
        let bot = bots.iter().find(|b| b.name == "typobot").unwrap();
        assert_eq!(bot.parser, "googel");
        assert!(!crate::parser::is_known(&bot.parser));
    }
}
