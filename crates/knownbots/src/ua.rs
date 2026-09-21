//! Case-sensitive User-Agent matching with word boundaries.
//!
//! Official bots publish a fixed casing (`Googlebot`, never
//! `googlebot`). Case folding would accept forgeries that merely
//! change case, so matching is byte-exact.
//!
//! A match must also sit on a word boundary — alphanumeric on either
//! side disqualifies (`MyGooglebot` does not match `Googlebot`).
//! Hyphen is a boundary, so `Applebot-Extended` contains the word
//! `Applebot`. When several markers match, the **longest** wins so
//! more-specific bots are preferred.
//!
//! Matching is a linear scan over the bot table (~40 entries). That
//! beats a per-byte index once longest-match forces a full pass
//! anyway, and keeps the hot path branch-predictable.

use crate::bot::Bot;

/// True when `c` is ASCII alphanumeric.
#[inline]
fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// Case-sensitive word-boundary search for `word` inside `text`.
pub fn contains_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let text_b = text.as_bytes();
    let word_b = word.as_bytes();
    let mut idx = 0;
    while idx + word_b.len() <= text_b.len() {
        if let Some(rel) = find_slice(&text_b[idx..], word_b) {
            let pos = idx + rel;
            let before_ok = pos == 0 || !is_alnum(text_b[pos - 1]);
            let after_ok =
                pos + word_b.len() == text_b.len() || !is_alnum(text_b[pos + word_b.len()]);
            if before_ok && after_ok {
                return true;
            }
            idx = pos + 1;
        } else {
            break;
        }
    }
    false
}

fn find_slice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Find the bot whose UA marker matches `ua`, preferring the longest
/// marker when several match. Returns the bot index into `bots`.
pub fn find_bot_by_ua(ua: &str, bots: &[Bot]) -> Option<usize> {
    if ua.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize)> = None; // (len, bot_idx)
    for (i, bot) in bots.iter().enumerate() {
        if bot.ua.is_empty() {
            continue;
        }
        if contains_word(ua, &bot.ua) {
            let len = bot.ua.len();
            match best {
                Some((best_len, _)) if best_len >= len => {}
                _ => best = Some((len, i)),
            }
        }
    }
    best.map(|(_, idx)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{Bot, BotKind};

    fn bot(name: &str, ua: &str) -> Bot {
        Bot {
            name: name.into(),
            kind: BotKind::SearchEngine,
            parser: "txt".into(),
            ua: ua.into(),
            vendor: "Test".into(),
            urls: Vec::new(),
            custom: Vec::new(),
            domains: Vec::new(),
            rdns: false,
            prefixes: parking_lot::RwLock::new(Vec::new()),
            rdns_cache: None,
            fail_cache: None,
        }
    }

    #[test]
    fn contains_word_exact() {
        assert!(contains_word(
            "Mozilla/5.0 (compatible; Googlebot/2.1)",
            "Googlebot"
        ));
    }

    #[test]
    fn contains_word_rejects_wrong_case() {
        assert!(!contains_word(
            "Mozilla/5.0 (compatible; googlebot/2.1)",
            "Googlebot"
        ));
    }

    #[test]
    fn contains_word_rejects_alphanumeric_prefix() {
        assert!(!contains_word("MyGooglebot/1.0", "Googlebot"));
    }

    #[test]
    fn contains_word_hyphen_is_boundary() {
        assert!(contains_word(
            "Mozilla/5.0 (compatible; Applebot-Extended/1.0)",
            "Applebot"
        ));
        assert!(contains_word(
            "Mozilla/5.0 (compatible; Applebot-Extended/1.0)",
            "Applebot-Extended"
        ));
    }

    #[test]
    fn longest_marker_wins() {
        let bots = vec![
            bot("applebot", "Applebot"),
            bot("applebot-extended", "Applebot-Extended"),
        ];
        let ua = "Mozilla/5.0 (compatible; Applebot-Extended/1.0)";
        let idx = find_bot_by_ua(ua, &bots).expect("match");
        assert_eq!(bots[idx].name, "applebot-extended");
    }

    #[test]
    fn inspection_tool_not_googlebot() {
        let bots = vec![
            bot("googlebot", "Googlebot"),
            bot("google-inspectiontool", "Google-InspectionTool"),
        ];
        let ua = "Mozilla/5.0 (compatible; Google-InspectionTool/1.0)";
        let idx = find_bot_by_ua(ua, &bots).expect("match");
        assert_eq!(bots[idx].name, "google-inspectiontool");
    }
}
