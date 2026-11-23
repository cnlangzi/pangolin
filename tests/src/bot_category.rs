//! Regression tests for the category vocabulary the host publishes
//! (issue #230).
//!
//! The vocabulary itself belongs to the `knownbots` crate: it states
//! the coarse bucket of every crawler in its own registry, and the
//! crate publishes the labels. What the host publishes on top of it
//! is the **contract downstream queries rely on** — the JSONL
//! `bot_category` of every row — and that contract has two halves
//! nothing else checks:
//!
//! 1. **Which bucket a given crawler lands in.** The crate's own
//!    contract test checks that a definition's category is *allowed*
//!    for its kind, and for `Monitor` it allows two: `monitoring` and
//!    `ads_bot`. So the crate cannot catch a release that moves the
//!    ad-quality crawlers out of `ads_bot` — and a bot that changes
//!    bucket changes the label of its JSONL rows, which is exactly
//!    the silent query breakage the crate's vocabulary was meant to
//!    prevent.
//! 2. **The badge class of a category.** The admin page paints the
//!    same column twice: the server-rendered rows use the Rust table,
//!    the rows the browser appends from the SSE stream use a
//!    JavaScript copy of it. The labels are now one source (the
//!    crate), but the two class tables are still two — and nothing
//!    kept them in step.
//!
//! The tests are hermetic: the registry is the one compiled into the
//! crate, and the JavaScript is read out of the admin template.

use std::collections::HashMap;

use pangolin_core::bot::{BotCategory, BotCategoryStyle, NEUTRAL_CATEGORY_CLASS};

use crate::common::read_repo_file;

/// The page that renders the bot log — and the JavaScript that
/// appends the live rows to it.
const BOTS_PAGE: &str = "crates/admin/templates/pages/logs_bots.html";

/// The bucket every crawler of the registry lands in.
///
/// This is the host's published contract: the JSONL `bot_category` of
/// a `googlebot` row is `search_engine`, and a downstream query that
/// filters on `ads_bot` has to keep finding the ad-quality crawlers.
///
/// The crate cannot check this by itself: `Monitor` allows both
/// `monitoring` and `ads_bot`, so a release that moves `adsbot` out
/// of the ads bucket breaks no test in the crate's repository — and
/// would break the queries of every operator who filters on it.
#[test]
fn the_registry_buckets_are_the_ones_the_rows_carry() {
    let bots = pangolin_core::bot::registry(None);

    for (name, category) in [
        ("googlebot", BotCategory::SearchEngine),
        ("bingbot", BotCategory::SearchEngine),
        ("semrushbot", BotCategory::SearchEngine),
        ("gptbot", BotCategory::AiBot),
        ("claudebot", BotCategory::AiBot),
        ("amazonbot", BotCategory::AiBot),
        ("facebookexternalhit", BotCategory::Social),
        ("telegrambot", BotCategory::Social),
        ("uptimerobot", BotCategory::Monitoring),
        ("pingdom", BotCategory::Monitoring),
        // The ad-quality crawlers: the one bucket the crate cannot
        // pin on its own, because `Monitor` allows two.
        ("adsbot", BotCategory::AdsBot),
        ("adsbot-mobile", BotCategory::AdsBot),
        ("mediapartners-google", BotCategory::AdsBot),
    ] {
        let bot = bots
            .iter()
            .find(|b| b.name == name)
            .unwrap_or_else(|| panic!("the registry has no bot named {name}"));
        assert_eq!(bot.category, category, "the bucket of {name}");
    }
}

/// The badge class of a category is the same on the server-rendered
/// rows and on the rows the browser appends.
///
/// The two render paths read the same column: the server paints it
/// with the Rust table, the browser with the JavaScript copy of it.
/// A divergence shows the same column in two colours on one page.
#[test]
fn the_badge_classes_match_the_live_tail_javascript() {
    let (classes, fallback) = js_category_classes(&read_repo_file(BOTS_PAGE));

    for category in BotCategory::ALL {
        let label = category.as_str();
        let js = classes
            .get(label)
            .unwrap_or_else(|| panic!("the JavaScript map has no entry for {label:?}"));
        assert_eq!(
            category.class(),
            js,
            "the Rust and the JavaScript class of {label:?} differ"
        );
    }

    // The fallback — the bucket the crate gains later — is the neutral
    // badge on both sides.
    assert_eq!(
        fallback, NEUTRAL_CATEGORY_CLASS,
        "the JavaScript default and the Rust fallback differ"
    );
}

/// The badge classes of the page's JavaScript: the wire label → class
/// map, and the class the `default:` branch returns.
fn js_category_classes(source: &str) -> (HashMap<String, String>, String) {
    let mut classes = HashMap::new();
    let mut fallback = String::new();
    let mut in_function = false;

    for line in source.lines() {
        let line = line.trim();
        if line.starts_with("function categoryClass(") {
            in_function = true;
            continue;
        }
        if !in_function {
            continue;
        }
        if line.starts_with('}') {
            break;
        }
        let Some(class) = line.split("return \"").nth(1) else {
            continue;
        };
        let class = class.split('"').next().unwrap_or_default();
        if let Some(label) = line
            .strip_prefix("case \"")
            .and_then(|rest| rest.split('"').next())
        {
            classes.insert(label.to_string(), class.to_string());
        } else if line.starts_with("default:") {
            fallback = class.to_string();
        }
    }

    assert!(
        !classes.is_empty(),
        "the page has no categoryClass() map — the badge would be unstyled"
    );
    assert!(
        !fallback.is_empty(),
        "the page's categoryClass() has no default branch"
    );
    (classes, fallback)
}
