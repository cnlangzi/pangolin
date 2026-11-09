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

/// A bot config file that was skipped at load time.
///
/// The operator dropped a file into `<root>/conf.d/` that the
/// loader could not turn into a bot — a YAML syntax error, or a
/// document without the required `name` / `ua` fields. The file is
/// skipped (the gateway keeps running) and reported here so the
/// admin UI can tell the operator *why* the bot they configured is
/// missing.
///
/// A malformed **or incomplete** **embedded** config is not an error
/// entry but a hard failure: it is a regression in this repository,
/// not an operator mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotConfigError {
    /// Path of the offending file. For an embedded config this is
    /// the compiled-in file name (e.g. `"googlebot.yaml"`).
    pub path: String,
    /// Why the file was skipped.
    pub message: String,
}

/// Outcome of parsing one bot YAML document.
enum ParsedBot {
    /// The document described a bot.
    Loaded(Box<Bot>),
    /// The document was skipped; the payload says why.
    Skipped(String),
}

/// Parse one bot YAML document.
///
/// A syntax error is an `Err` — the caller decides whether that is
/// fatal (embedded config) or skippable (operator override). A
/// document that parses but lacks `name` / `ua` is a `Skipped` with
/// the reason, so the operator learns why their bot is missing.
fn parse_yaml(data: &str, filename: &str) -> Result<ParsedBot> {
    let cfg: BotConfigFile =
        serde_yaml::from_str(data).with_context(|| format!("parse bot config {filename}"))?;
    if cfg.name.is_empty() {
        return Ok(ParsedBot::Skipped(
            "missing required 'name' field".to_string(),
        ));
    }
    if cfg.ua.is_empty() {
        return Ok(ParsedBot::Skipped(
            "missing required 'ua' field".to_string(),
        ));
    }
    Ok(ParsedBot::Loaded(Box::new(Bot::from_config(cfg)?)))
}

/// Load the embedded (compiled-in) bot configs.
///
/// A malformed **or incomplete** embedded config is **fatal**: it is
/// a regression in this repository, not an operator error, and must
/// never reach production silently. Skipping it would drop the bot
/// from the registry — its traffic would stop being logged, and
/// nothing on the running gateway would say so.
fn load_embedded(configs: &[(&str, &str)]) -> Result<Vec<Bot>> {
    use std::collections::HashMap;

    let mut by_name: HashMap<String, Bot> = HashMap::new();
    for (filename, data) in configs {
        match parse_yaml(data, filename)? {
            ParsedBot::Loaded(bot) => {
                by_name.insert(bot.name.clone(), *bot);
            }
            ParsedBot::Skipped(reason) => {
                anyhow::bail!("embedded bot config {filename} is incomplete: {reason}");
            }
        }
    }
    Ok(by_name.into_values().collect())
}

/// Load embedded configs, then overlay any `*.yaml` / `*.yml` from
/// `override_dir/conf.d` (same-name wins).
///
/// Operator files that cannot be turned into a bot are **skipped**,
/// logged as a warning, and returned in the error list — a typo in
/// an override file must not take the gateway down. A malformed or
/// incomplete *embedded* config stays fatal (see [`load_embedded`]).
pub fn load_bots_with_errors(
    override_dir: Option<&Path>,
) -> Result<(Vec<Bot>, Vec<BotConfigError>)> {
    use std::collections::HashMap;

    let mut errors = Vec::new();
    let mut by_name: HashMap<String, Bot> = load_embedded(EMBEDDED_CONFIGS)?
        .into_iter()
        .map(|bot| (bot.name.clone(), bot))
        .collect();

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
                let label = path.display().to_string();
                let data = match std::fs::read_to_string(&path) {
                    Ok(data) => data,
                    Err(e) => {
                        // An unreadable file is as much an operator
                        // error as an unparsable one — skip it
                        // instead of refusing to start.
                        let reason = format!("read {label}: {e}");
                        log::warn!("knownbots: skip bot config {label}: {e}");
                        errors.push(BotConfigError {
                            path: label,
                            message: reason,
                        });
                        continue;
                    }
                };
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("custom.yaml");
                match parse_yaml(&data, name) {
                    Ok(ParsedBot::Loaded(bot)) => {
                        log::info!("knownbots: custom config overrides bot {}", bot.name);
                        by_name.insert(bot.name.clone(), *bot);
                    }
                    Ok(ParsedBot::Skipped(reason)) => {
                        log::warn!("knownbots: skip bot config {label}: {reason}");
                        errors.push(BotConfigError {
                            path: label,
                            message: reason,
                        });
                    }
                    Err(e) => {
                        let reason = format!("{e:#}");
                        log::warn!("knownbots: skip bot config {label}: {reason}");
                        errors.push(BotConfigError {
                            path: label,
                            message: reason,
                        });
                    }
                }
            }
        }
    }

    let mut bots: Vec<Bot> = by_name.into_values().collect();
    // Stable order for tests / debugging.
    bots.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((bots, errors))
}

/// Load embedded configs, then overlay any `*.yaml` / `*.yml` from
/// `override_dir/conf.d` (same-name wins).
///
/// Skipped operator files are logged as warnings. Use
/// [`load_bots_with_errors`] to also get them back.
pub fn load_bots(override_dir: Option<&Path>) -> Result<Vec<Bot>> {
    let (bots, _errors) = load_bots_with_errors(override_dir)?;
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

    /// A typo in an operator override file must not take the
    /// gateway down: the file is skipped, the rest of the registry
    /// loads, and the file is reported so the admin UI can show
    /// *why* the bot the operator configured is missing.
    #[test]
    fn malformed_override_file_is_skipped_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        let broken = conf.join("broken.yaml");
        std::fs::write(&broken, "name: brokenbot\n  ua: [unclosed\n").unwrap();
        std::fs::write(
            conf.join("goodbot.yaml"),
            "name: goodbot\nua: \"GoodBot\"\nvendor: \"Example\"\n",
        )
        .unwrap();

        let (bots, errors) = load_bots_with_errors(Some(dir.path())).unwrap();
        // The malformed file is absent from the registry …
        assert!(!bots.iter().any(|b| b.name == "brokenbot"));
        // … the well-formed one is not.
        assert!(bots.iter().any(|b| b.name == "goodbot"));
        // … and the embedded registry still loaded.
        assert!(bots.iter().any(|b| b.name == "googlebot"));
        // One config error, naming the file and the parse failure.
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert_eq!(errors[0].path, broken.display().to_string());
        assert!(
            errors[0].message.contains("parse bot config"),
            "message must name the parse failure: {}",
            errors[0].message
        );
    }

    /// A file that parses but lacks `name` / `ua` is skipped too —
    /// and reported, so the operator learns why their bot is
    /// missing instead of just not seeing it.
    #[test]
    fn incomplete_override_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        let path = conf.join("nameless.yaml");
        // An empty `name` parses but carries no identity.
        std::fs::write(&path, "name: \"\"\nua: \"Nameless\"\n").unwrap();

        let (bots, errors) = load_bots_with_errors(Some(dir.path())).unwrap();
        assert!(!bots.iter().any(|b| b.ua == "Nameless"));
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert_eq!(errors[0].path, path.display().to_string());
        assert!(
            errors[0].message.contains("name"),
            "message must name the missing field: {}",
            errors[0].message
        );
    }

    /// A YAML that omits a required field entirely is a parse
    /// failure — reported the same way, not fatal.
    #[test]
    fn override_file_without_a_name_field_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        let path = conf.join("nameless.yaml");
        std::fs::write(&path, "ua: \"Nameless\"\n").unwrap();

        let (bots, errors) = load_bots_with_errors(Some(dir.path())).unwrap();
        assert!(!bots.iter().any(|b| b.ua == "Nameless"));
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert_eq!(errors[0].path, path.display().to_string());
        assert!(
            errors[0].message.contains("name"),
            "message must name the missing field: {}",
            errors[0].message
        );
    }

    /// A malformed **embedded** config is a regression in this
    /// repository, not an operator error — it stays fatal so a
    /// broken build cannot reach production silently.
    #[test]
    fn malformed_embedded_config_is_fatal() {
        let Err(err) = load_embedded(&[("broken.yaml", "name: brokenbot\n  ua: [unclosed\n")])
        else {
            panic!("a broken embedded config must fail the loader");
        };
        assert!(
            format!("{err:#}").contains("parse bot config broken.yaml"),
            "error must name the file: {err:#}"
        );
    }

    /// An embedded config that parses but lacks `name` / `ua` is
    /// just as much a repository regression: skipping it would drop
    /// the bot from the registry and stop its traffic from being
    /// logged, with nothing on the running gateway to say so.
    #[test]
    fn incomplete_embedded_config_is_fatal() {
        let Err(err) = load_embedded(&[("googlebot.yaml", "name: \"\"\nua: \"Googlebot\"\n")])
        else {
            panic!("an incomplete embedded config must fail the loader");
        };
        assert!(
            format!("{err:#}").contains("googlebot.yaml"),
            "error must name the file: {err:#}"
        );
    }

    /// Every config this crate ships must be complete — the check
    /// that keeps the fatal path above from firing in production.
    #[test]
    fn every_shipped_embedded_config_is_complete() {
        let bots = load_embedded(EMBEDDED_CONFIGS)
            .expect("every shipped embedded config must be complete");
        assert!(bots.len() >= 40, "loaded {} bots", bots.len());
        assert!(bots.iter().any(|b| b.name == "googlebot"));
    }

    /// True when a `domains:` entry can still match a PTR record.
    ///
    /// [`crate::rdns::match_domain`] normalises every entry by
    /// stripping its trailing dots and lower-casing it, then compares
    /// it against the hostname the same way. An entry that is empty
    /// after that (the empty string, or nothing but dots) can never
    /// equal a hostname, and one carrying whitespace can never be a
    /// suffix of one — so a bot whose `domains:` holds only such
    /// entries is as unmatchable as a bot with no list at all.
    fn domain_is_usable(domain: &str) -> bool {
        let d = domain.trim_end_matches('.');
        !d.is_empty() && !d.contains(char::is_whitespace)
    }

    /// [`domain_is_usable`] must agree with the comparison it stands in
    /// for: a domain it calls unusable has to be one that no PTR record
    /// can ever match.
    #[test]
    fn domain_usability_mirrors_the_ptr_comparison() {
        // A normal domain, with and without a trailing dot — and it
        // does match a PTR record.
        assert!(domain_is_usable("example.com"));
        assert!(domain_is_usable("example.com."));
        assert!(crate::rdns::match_domain(
            "crawl-66-249-66-1.example.com",
            &["example.com".to_string()]
        ));

        // Nothing left after the trailing dots are stripped — and no PTR
        // record can match such an entry.
        for unusable in ["", ".", "..."] {
            assert!(!domain_is_usable(unusable), "{unusable:?}");
            assert!(
                !crate::rdns::match_domain("crawl.example.com", &[unusable.to_string()]),
                "{unusable:?} must not match a hostname"
            );
        }

        // Whitespace can never be part of a hostname.
        assert!(!domain_is_usable(" example.com"));
        assert!(!domain_is_usable("example .com"));
    }

    /// The contract every embedded bot config must satisfy.
    ///
    /// Rules 3 to 6 can be broken **silently**: the loader only warns
    /// (or never reads the file at all), the bot then never verifies a
    /// request, its traffic never reaches the bot log, and nothing on
    /// the running gateway reports an error. Rule 1 is silent in its
    /// *duplicate* half — the loader keeps the last bot of a duplicated
    /// name — and loud in its *empty* half, like rule 2: `load_embedded`
    /// treats an incomplete embedded config as fatal and the gateway
    /// refuses to start.
    ///
    /// The contract itself is written down in `docs/design/knownbots.md`
    /// — the two must agree.
    ///
    /// Every violation is collected, so one run reports every problem
    /// instead of only the first.
    #[test]
    fn embedded_bot_configs_satisfy_the_contract() {
        use std::collections::{HashMap, HashSet};

        let mut violations: Vec<String> = Vec::new();
        // Registry id → the file that declared it, so a duplicate names both.
        let mut seen_names: HashMap<String, &str> = HashMap::new();

        for &(filename, data) in EMBEDDED_CONFIGS {
            // The document as the crate's own config struct sees it. A
            // document that does not even fit the struct is broken in a
            // way the loader reports loudly (it refuses to start), so
            // say what serde says — the field it names is the defect.
            let cfg: BotConfigFile = match serde_yaml::from_str(data) {
                Ok(cfg) => cfg,
                Err(e) => {
                    violations.push(format!("{filename}: not a valid bot config: {e:#}"));
                    continue;
                }
            };

            // Rule 1 — `name` is non-empty and unique across the
            // registry. The loader keeps the *last* bot of a duplicated
            // name, so without this rule one bot silently replaces the
            // other.
            if cfg.name.is_empty() {
                violations.push(format!("{filename}: missing name"));
            } else if let Some(first) = seen_names.insert(cfg.name.clone(), filename) {
                violations.push(format!(
                    "{filename}: duplicate bot name {:?} (already declared by {first})",
                    cfg.name
                ));
            }

            // Rule 2 — `ua` is non-empty. An empty marker matches no
            // User-Agent, so the bot would never verify.
            if cfg.ua.is_empty() {
                violations.push(format!("{filename}: missing ua"));
            }

            // Rule 4 — an RDNS bot needs a domain list it can actually
            // match. Without one no PTR record can ever match, so the
            // bot is permanently unverified.
            if cfg.rdns {
                if cfg.domains.is_empty() {
                    violations.push(format!("{filename}: rdns bot without domains"));
                } else if !cfg.domains.iter().any(|d| domain_is_usable(d)) {
                    violations.push(format!(
                        "{filename}: rdns bot with no usable domain — every entry matches no PTR record"
                    ));
                }
            }

            // Rule 5 — every `custom:` entry must be a CIDR. The loader
            // skips a bad one with a warning, so the bot would silently
            // cover less address space than the config claims.
            for cidr in &cfg.custom {
                if let Err(e) = cidr.parse::<IpNet>() {
                    violations.push(format!("{filename}: invalid custom CIDR {cidr:?}: {e}"));
                }
            }

            // Rule 3 — the parser the loader will use for the download
            // is the *effective* one: an absent `parser:` means `txt`.
            // An unknown parser only warns at load time; the list is
            // then parsed as plain text and the refresh ends as
            // "broken".
            let bot = match Bot::from_config(cfg) {
                Ok(bot) => bot,
                Err(e) => {
                    violations.push(format!("{filename}: the loader rejects it: {e:#}"));
                    continue;
                }
            };
            if !bot.urls.is_empty() && !crate::parser::is_known(&bot.parser) {
                violations.push(format!(
                    "{filename}: unknown parser {:?} for a URL-backed bot",
                    bot.parser
                ));
            }
        }

        // Rule 6 — every config file in `conf.d/` must be compiled into
        // the binary. A file that is not listed in `EMBEDDED_CONFIGS` is
        // never parsed, so it does nothing at all.
        let conf_d = Path::new(env!("CARGO_MANIFEST_DIR")).join("conf.d");
        let embedded: HashSet<&str> = EMBEDDED_CONFIGS.iter().map(|(name, _)| *name).collect();
        let mut files: Vec<String> = std::fs::read_dir(&conf_d)
            .expect("conf.d/ must be readable")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".yaml") || name.ends_with(".yml"))
            .collect();
        files.sort();
        for file in files {
            if !embedded.contains(file.as_str()) {
                violations.push(format!(
                    "{file}: config file not embedded — list it in EMBEDDED_CONFIGS \
                     (crates/knownbots/src/bot.rs)"
                ));
            }
        }

        assert!(
            violations.is_empty(),
            "the embedded bot configs break the contract ({} problem(s)):\n  - {}",
            violations.len(),
            violations.join("\n  - "),
        );
    }
}
