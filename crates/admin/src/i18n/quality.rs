//! The Chinese quality gates.
//!
//! A machine translation is easy to spot: the wording is stiff, the
//! punctuation is half-width, the Latin words are glued to the Chinese
//! ones, or the string was simply not translated at all.  Three of
//! those four are decidable by a machine, so they are decided by a
//! machine — here.
//!
//! What the gates check:
//!
//! 1. **漏译** — a Chinese value that is *identical* to the English one
//!    is a string nobody translated.  The whitelist
//!    [`IDENTICAL_OK`] holds the keys where that is deliberate.
//! 2. **标点** — a Chinese value may not contain half-width
//!    punctuation.  Chinese typography uses the full-width forms
//!    （），？！；：.
//! 3. **空格** — a Chinese character glued to an ASCII letter reads
//!    badly ("DNS记录").  "DNS 记录" is the Chinese way.
//!
//! What the gates deliberately do **not** check: whether the Chinese
//! is *good*.  A regex cannot tell "最近 {n} 条事件" from "最后 {n}
//! 个事件(s)".  That is what the glossary and the writing rules in
//! `docs/admin/i18n.md` are for — and what a human reviewer is for.
//!
//! A value that only contains ASCII is exempt from gates 2 and 3: a
//! technical identifier (`DNS`, `User-Agent`, `Cloudflare`) has no
//! Chinese punctuation to convert and no Chinese word to space out.
//! Such a value still has to be whitelisted for gate 1.

use crate::i18n::Lang;
use crate::i18n::catalog::catalog_for;

/// The keys whose Chinese text is deliberately identical to the English
/// one.
///
/// The list is keyed, not valued: a new key that happens to carry an
/// untranslatable string has to be registered here **once**, which
/// forces the translator to look at it.  A whitelist of values would
/// let a new key slip through for free.
///
/// Every entry needs a reason:
///
/// - `common.provider.cloudflare.label` — the product name.
/// - `common.user_agent` — the name of the HTTP header.
/// - `login.tagline` — the Chinese project tagline; the English
///   catalog already carries it, so translating it again would only
///   invent a second spelling of the same slogan.
pub const IDENTICAL_OK: &[&str] = &[
    "common.provider.cloudflare.label",
    "common.user_agent",
    "login.tagline",
];

/// The half-width punctuation a Chinese value may not contain.
///
/// The Chinese forms are `（` `）` `，` `？` `！` `；` `：`.
const HALF_WIDTH_PUNCTUATION: &[char] = &['(', ')', ',', '?', '!', ';', ':'];

/// The number of Chinese characters above which a value is reported as
/// possibly too long.
///
/// A hint, not a failure: some sentences are naturally long, and a
/// hard limit would push the translator into writing an incomplete
/// sentence just to pass the test.
pub const LONG_TEXT_HANZI: usize = 20;

/// Whether `c` is a Chinese ideograph.
///
/// The block is the one the space gate uses: the full-width
/// punctuation (`。`, `，`, `（`) lives outside it, and it is *correct*
/// to write "。Cloudflare" without a space.
fn is_hanzi(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// Whether `value` contains at least one Chinese ideograph.
fn has_hanzi(value: &str) -> bool {
    value.chars().any(is_hanzi)
}

/// The number of Chinese ideographs in `value`.
fn hanzi_count(value: &str) -> usize {
    value.chars().filter(|c| is_hanzi(*c)).count()
}

/// One key of the Chinese catalog, with the English text it was
/// translated from.
struct Pair {
    key: &'static str,
    english: &'static str,
    chinese: &'static str,
}

/// The keys that exist in both catalogs, sorted by key.
///
/// A key only the English catalog has is not translated *yet* — that
/// is the normal state of a partially translated catalog and not the
/// gate's business.
fn shared_keys() -> Vec<Pair> {
    let english = catalog_for(Lang::En);
    let chinese = catalog_for(Lang::ZhCn);
    let mut pairs: Vec<Pair> = chinese
        .iter()
        .filter_map(|(key, value)| {
            english.get(key).map(|en| Pair {
                key: key.as_str(),
                english: en.as_str(),
                chinese: value.as_str(),
            })
        })
        .collect();
    pairs.sort_by(|a, b| a.key.cmp(b.key));
    pairs
}

/// The keys whose Chinese text is identical to the English one and
/// that are not whitelisted — the untranslated strings.
pub fn untranslated_keys() -> Vec<&'static str> {
    shared_keys()
        .into_iter()
        .filter(|pair| pair.chinese == pair.english && !IDENTICAL_OK.contains(&pair.key))
        .map(|pair| pair.key)
        .collect()
}

/// The keys whose Chinese text contains half-width punctuation.
pub fn half_width_punctuation_keys() -> Vec<&'static str> {
    shared_keys()
        .into_iter()
        .filter(|pair| {
            has_hanzi(pair.chinese)
                && pair
                    .chinese
                    .chars()
                    .any(|c| HALF_WIDTH_PUNCTUATION.contains(&c))
        })
        .map(|pair| pair.key)
        .collect()
}

/// The keys whose Chinese text glues a Chinese character to an ASCII
/// letter without a space.
pub fn missing_space_keys() -> Vec<&'static str> {
    shared_keys()
        .into_iter()
        .filter(|pair| {
            if !has_hanzi(pair.chinese) {
                return false;
            }
            let chars: Vec<char> = pair.chinese.chars().collect();
            chars.windows(2).any(|window| {
                (is_hanzi(window[0]) && window[1].is_ascii_alphabetic())
                    || (window[0].is_ascii_alphabetic() && is_hanzi(window[1]))
            })
        })
        .map(|pair| pair.key)
        .collect()
}

/// The keys whose Chinese text holds more than [`LONG_TEXT_HANZI`]
/// Chinese characters, with the count.
pub fn long_keys() -> Vec<(&'static str, usize)> {
    shared_keys()
        .into_iter()
        .filter_map(|pair| {
            let count = hanzi_count(pair.chinese);
            (count > LONG_TEXT_HANZI).then_some((pair.key, count))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A readable list of the offending keys and their Chinese text.
    fn offenders(keys: &[&str]) -> String {
        let chinese = catalog_for(Lang::ZhCn);
        keys.iter()
            .map(|key| match chinese.get(*key) {
                Some(value) => format!("{key} = {value:?}"),
                None => format!("{key} = <missing>"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Gate 1 — 漏译检测.
    ///
    /// A Chinese value that is byte for byte the English one is a
    /// string nobody translated.  The whitelist exists for the few
    /// keys where that is deliberate.
    #[test]
    fn no_chinese_string_is_left_untranslated() {
        let keys = untranslated_keys();
        assert!(
            keys.is_empty(),
            "these Chinese strings are identical to the English ones — translate \
             them or add them to IDENTICAL_OK:\n{}",
            offenders(&keys)
        );
    }

    /// Gate 2 — 标点检测.
    ///
    /// Chinese typography uses the full-width punctuation; the
    /// half-width forms are the fingerprint of a machine translation.
    #[test]
    fn chinese_strings_use_full_width_punctuation() {
        let keys = half_width_punctuation_keys();
        assert!(
            keys.is_empty(),
            "these Chinese strings contain half-width punctuation:\n{}",
            offenders(&keys)
        );
    }

    /// Gate 3 — 空格检测.
    ///
    /// "DNS记录" is not Chinese typography; "DNS 记录" is.
    #[test]
    fn chinese_strings_space_out_latin_words() {
        let keys = missing_space_keys();
        assert!(
            keys.is_empty(),
            "these Chinese strings glue a Chinese character to a Latin word \
             without a space:\n{}",
            offenders(&keys)
        );
    }

    /// The whitelist has to stay honest: an entry that names a key
    /// which is *not* identical any more is a stale entry, and a stale
    /// entry hides the next untranslated string.
    #[test]
    fn the_whitelist_names_only_identical_strings() {
        let chinese = catalog_for(Lang::ZhCn);
        let english = catalog_for(Lang::En);
        for key in IDENTICAL_OK {
            assert!(
                chinese.contains_key(*key),
                "the whitelist names {key}, which the Chinese catalog does not have"
            );
            assert_eq!(
                chinese.get(*key),
                english.get(*key),
                "the whitelist names {key}, but the two catalogs disagree about it"
            );
        }
    }

    /// The whitelist names real keys — a typo would silently whitelist
    /// nothing.
    #[test]
    fn the_whitelist_names_keys_of_the_catalog() {
        let english = catalog_for(Lang::En);
        for key in IDENTICAL_OK {
            assert!(
                english.contains_key(*key),
                "the whitelist names {key}, which is not a catalog key"
            );
        }
    }

    /// The long-text hint.
    ///
    /// A hint, not a failure: the test prints the offending keys so a
    /// reviewer can look at them, and it always passes.
    #[test]
    fn long_chinese_strings_are_reported() {
        for (key, count) in long_keys() {
            let chinese = catalog_for(Lang::ZhCn);
            eprintln!(
                "i18n: {key} holds {count} Chinese characters — check that it is \
                 not too long: {:?}",
                chinese.get(key).unwrap_or(&String::new())
            );
        }
    }

    /// Both languages are offered once the Chinese catalog has text.
    ///
    /// The switcher only renders when there is more than one language
    /// to choose from, so this is what makes the Chinese UI reachable
    /// at all.
    #[test]
    fn both_languages_are_available() {
        let available = Lang::available();
        assert!(
            available.contains(&Lang::En),
            "English must be offered: {available:?}"
        );
        assert!(
            available.contains(&Lang::ZhCn),
            "Chinese must be offered: {available:?}"
        );
    }
}
