//! The language catalogs.
//!
//! One JSON file per language, embedded at compile time with
//! `include_str!` and parsed once per process into a
//! `HashMap<String, String>`. A file that is not valid JSON is logged
//! and treated as an empty catalog — the interface then falls back
//! to English instead of failing the request.
//!
//! The English catalog is the fallback for every other language: a
//! key a translation has not covered yet still renders in English.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::Lang;

/// The English catalog. Also the fallback for every other language.
const EN_JSON: &str = include_str!("../../i18n/en.json");

/// The Simplified Chinese catalog.
///
/// Translated area by area: the areas that are done are listed in
/// [`COMPLETED_AREAS`], and every other area falls back to the
/// English catalog until its translation issue lands.
const ZH_CN_JSON: &str = include_str!("../../i18n/zh-CN.json");

/// A parsed catalog: key → text.
pub type Catalog = HashMap<String, String>;

static EN: OnceLock<Catalog> = OnceLock::new();
static ZH_CN: OnceLock<Catalog> = OnceLock::new();

/// The areas a catalog key may live in.
///
/// The list is closed on purpose: a key that names an area nobody
/// declared is a typo, and the gate test turns it red.
pub const AREAS: &[&str] = &[
    "nav",
    "common",
    "login",
    "dashboard",
    "components",
    "sites",
    "domains",
    "tunnels",
    "certs",
    "dns",
    "ratelimit",
    "settings",
    "logs",
    "bots",
    "referers",
    "traffic",
    "time",
    "activity",
    "lang",
    "js",
];

/// The areas whose translation is complete.
///
/// A translation issue adds its areas here once every key of the area
/// exists in every non-empty catalog. The completeness gate test then
/// fails if one of those keys goes missing — so "translated" cannot
/// silently rot into "half translated".
///
/// Issue #179 translated the global chrome: the navigation, the
/// shared labels, the login page, the dashboard, the shared
/// components, the relative times, the activity feed and the
/// client-side strings. The `lang` area is listed too: the language
/// switcher shows every language in its own name, so there is
/// nothing to translate in it — but the two catalogs have to agree
/// about that.
///
/// Issue #180 translated the core configuration objects: the sites,
/// the domains and the tunnels — their forms, their tables and the
/// validation errors the routes report back to the operator.
///
/// Issue #181 translated the infrastructure and the policy: the
/// certificates, the DNS providers, the rate limit and the system
/// settings — the pages an operator reads when something went wrong.
///
/// Issue #182 translated the logs: the live access log and the bot
/// logs with their history and their stats. The two live in one
/// issue because they share the same vocabulary — a request, a bot,
/// a status code, a referrer — and a term that is translated two
/// ways on two pages is how an interface starts to feel untranslated.
pub const COMPLETED_AREAS: &[&str] = &[
    "activity",
    "bots",
    "certs",
    "common",
    "components",
    "dashboard",
    "dns",
    "domains",
    "js",
    "lang",
    "login",
    "logs",
    "nav",
    "ratelimit",
    "settings",
    "sites",
    "time",
    "tunnels",
];

/// The catalog of one language.
pub fn catalog_for(lang: Lang) -> &'static Catalog {
    match lang {
        Lang::En => en(),
        Lang::ZhCn => zh_cn(),
    }
}

/// The English catalog.
pub fn en() -> &'static Catalog {
    EN.get_or_init(|| parse(EN_JSON, "en"))
}

/// The Simplified Chinese catalog.
pub fn zh_cn() -> &'static Catalog {
    ZH_CN.get_or_init(|| parse(ZH_CN_JSON, "zh-CN"))
}

/// Parse a catalog file. A malformed file is an empty catalog, not a
/// panic: the request must still render (in English, not a panic).
fn parse(source: &str, label: &str) -> Catalog {
    match serde_json::from_str::<Catalog>(source) {
        Ok(catalog) => catalog,
        Err(err) => {
            log::error!("i18n: catalog {label} is not valid JSON: {err}");
            Catalog::new()
        }
    }
}

/// Whether `key` has the shape every catalog key must have.
///
/// `area.item`, lower-case snake_case, at least two segments. The
/// first segment starts with a letter, the others may start with a
/// digit or an underscore.
pub fn is_valid_key(key: &str) -> bool {
    let mut segments = key.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    let mut first_chars = first.chars();
    match first_chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    if !first_chars.all(is_key_char) {
        return false;
    }
    let mut seen_rest = false;
    for segment in segments {
        if segment.is_empty() || !segment.chars().all(is_key_char) {
            return false;
        }
        seen_rest = true;
    }
    seen_rest
}

/// Whether `c` may appear in a catalog key.
fn is_key_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
}

/// The area of a key — the part before the first dot.
pub fn area_of(key: &str) -> &str {
    key.split('.').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape_is_enforced() {
        assert!(is_valid_key("nav.sites"));
        assert!(is_valid_key("sites.delete.confirm"));
        assert!(is_valid_key("time.seconds_ago"));
        assert!(is_valid_key("a.b"));
        // A single segment is not a key.
        assert!(!is_valid_key("nav"));
        assert!(!is_valid_key(""));
        // The first segment must start with a letter.
        assert!(!is_valid_key("_nav.sites"));
        assert!(!is_valid_key("1nav.sites"));
        // Upper case, spaces and dashes are not allowed.
        assert!(!is_valid_key("Nav.sites"));
        assert!(!is_valid_key("nav.my key"));
        assert!(!is_valid_key("nav.my-key"));
        // An empty segment is not a key.
        assert!(!is_valid_key("nav..sites"));
        assert!(!is_valid_key("nav."));
    }

    #[test]
    fn catalogs_parse() {
        // Both files are embedded at compile time; an empty JSON
        // object is a valid catalog.
        assert!(en().contains_key("nav.sites"));
        // The Chinese catalog has text: the areas issue #179
        // translated are in it, and the switcher only shows the
        // language when its catalog is not empty.
        assert!(zh_cn().contains_key("nav.sites"));
    }

    #[test]
    fn english_catalog_keys_are_well_formed() {
        for key in en().keys() {
            assert!(is_valid_key(key), "key {key} does not match the key shape");
            assert!(
                AREAS.contains(&area_of(key)),
                "key {key} names an area that is not declared"
            );
        }
    }

    /// Whether a value looks like it carries an HTML tag.
    ///
    /// A `<` followed by a letter or a slash — not a comparison
    /// operator, not a stray arrow.
    fn contains_html_tag(value: &str) -> bool {
        let bytes = value.as_bytes();
        bytes
            .windows(2)
            .any(|w| w[0] == b'<' && (w[1].is_ascii_alphabetic() || w[1] == b'/'))
    }

    /// A catalog value is plain text — it carries no HTML.
    ///
    /// askama escapes `{{ }}`, so a tag in a value is *shown* to the
    /// operator as text instead of doing what it looks like it does.
    /// The domains form's DNS hint once smuggled a closing `</a>` into
    /// the English value to compensate for a tag the template forgot:
    /// the tag in the value did nothing (it was escaped) and the
    /// operator read a literal "</a>." on the page.
    #[test]
    fn no_catalog_value_carries_html() {
        for (lang, catalog) in [(Lang::En, en()), (Lang::ZhCn, zh_cn())] {
            for (key, value) in catalog {
                assert!(
                    !contains_html_tag(value),
                    "the {} catalog carries HTML in {key}: {value:?}",
                    lang.code()
                );
            }
        }
        // The predicate itself: a tag is a tag, a comparison is not.
        assert!(contains_html_tag("</a>"));
        assert!(contains_html_tag("<b>bold</b>"));
        assert!(!contains_html_tag("a < b"));
        assert!(!contains_html_tag("<= 5"));
    }
}
