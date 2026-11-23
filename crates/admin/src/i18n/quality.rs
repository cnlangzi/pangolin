//! The Chinese quality gates.
//!
//! A machine translation is easy to spot: the wording is stiff, the
//! punctuation is half-width, the Latin words are glued to the Chinese
//! ones, or the string was simply not translated at all.  Three of
//! those four are decidable by a machine, so they are decided by a
//! machine — here.  The fourth gate looks the other way: the
//! *templates* must not spell English prose out instead of asking the
//! catalog for it.
//!
//! What the gates check:
//!
//! 1. **漏译** — a Chinese value that is *identical* to the English one
//!    is a string nobody translated.  The whitelist
//!    [`IDENTICAL_OK`] holds the keys where that is deliberate.
//! 2. **标点** — a Chinese value may not contain half-width
//!    punctuation.  Chinese typography uses the full-width forms
//!    （），？！；：.
//! 3. **空格** — a Chinese character glued to a Latin word, a number
//!    or a placeholder reads badly ("DNS记录", "共3条").  "DNS 记录"
//!    is the Chinese way.
//! 4. **残留英文** — a template that carries English prose the
//!    catalog was never asked for.  The whitelist
//!    [`ALLOWED_ENGLISH_TEXT`] holds the literals that are *meant* to
//!    stay English: the brand name, the protocol abbreviations, the
//!    third-party site names, the configuration keys.
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
//!
//! The predicates are free functions over a *value*, and the catalog
//! walkers call them.  That split is what makes the gates testable on
//! their own: a gate that reads the shipped catalog can only ever be
//! exercised by the data it is checking, so a broken predicate would
//! leave the suite green.  The unit tests below therefore feed the
//! predicates synthetic strings.

use crate::i18n::Lang;
use crate::i18n::catalog::catalog_for;
use crate::i18n::{crate_root, files_with_extension};

/// The keys whose Chinese text is deliberately identical to the English
/// one, and why.
///
/// The list is keyed, not valued: a new key that happens to carry an
/// untranslatable string has to be registered here **once**, which
/// forces the translator to look at it.  A whitelist of values would
/// let a new key slip through for free.
///
/// Every entry carries its reason, so a reviewer can tell a
/// deliberate decision from an accident — and so an entry that has
/// stopped being true is obvious.
const IDENTICAL_OK: &[(&str, &str)] = &[
    // The product name of the DNS provider.
    ("common.provider.cloudflare.label", "the product's own name"),
    // The name of the HTTP request header.
    ("common.user_agent", "the name of the HTTP header"),
    // The cloud vendor's own console field names: the operator has to
    // find the very same words there.
    (
        "dns.error.label_access_key_secret",
        "the cloud vendor's console field name",
    ),
    (
        "dns.error.label_secret_key",
        "the cloud vendor's console field name",
    ),
    (
        "dns.form.access_key_id",
        "the cloud vendor's console field name",
    ),
    (
        "dns.form.access_key_secret",
        "the cloud vendor's console field name",
    ),
    (
        "dns.form.secret_id",
        "the cloud vendor's console field name",
    ),
    (
        "dns.form.secret_key",
        "the cloud vendor's console field name",
    ),
    // The same two field names, in the shorter spelling the Tencent
    // console uses.
    (
        "dns.form.tencent_hint",
        "the cloud vendor's console field name",
    ),
    // The Cloudflare permission path — an API path, not prose.
    ("dns.form.permission_2", "the API path of the permission"),
    // The project's own tagline: the English catalog already carries
    // it, so translating it again would only invent a second spelling
    // of the same slogan.
    ("login.tagline", "the project tagline, already in Chinese"),
    // The rule kind is an enumeration value that lands in the database
    // and in the rule matcher; the Chinese explanation lives in the
    // long form next to it.
    (
        "ratelimit.kind_blacklist_short",
        "the rule kind's enumeration value",
    ),
    (
        "ratelimit.kind_whitelist_short",
        "the rule kind's enumeration value",
    ),
    // The HTTP status code.
    ("ratelimit.sites.throttled", "the HTTP status code"),
    // The percentile labels. The traffic card shows `p95` next to them,
    // hard-coded in the template, and the operator reads the whole
    // family as one notation; translating two of the three would break
    // the series.
    ("traffic.p50", "the percentile label, part of one notation"),
    ("traffic.p99", "the percentile label, part of one notation"),
];

/// The literal strings that may appear in a template in English.
///
/// The residual-English gate is a whitelist and a failure, not a
/// hint: the point of the gate is to *kill* the residue.  Every entry
/// here is therefore one explicit confirmation that the literal is
/// meant to stay English — the brand name, a protocol abbreviation, a
/// third-party site name, a configuration key, a file name.
///
/// The list holds **literals**, not patterns.  A literal may be
/// matched as a whole (a whole text node, or a whole attribute
/// value) or as one word of a text node: "— Pangolin" is allowed
/// because "Pangolin" is, and "&lt;h1&gt;Forbidden&lt;/h1&gt;" is
/// allowed because it is listed as a whole.
///
/// The entries are grouped by what they are; the group comment is the
/// reason.
const ALLOWED_ENGLISH_TEXT: &[&str] = &[
    // The brand and the interface's own name.
    "Pangolin",
    // The example user name on the login form.
    "admin",
    // The separator the page titles use.
    "—",
    // Protocol and algorithm abbreviations.
    "ACME",
    "DNS",
    "HTTP",
    "IP",
    "TCP",
    "TLS",
    // HTTP methods.
    "DELETE",
    "GET",
    "HEAD",
    "PATCH",
    "POST",
    "PUT",
    // HTTP header names.
    "Referer",
    "User-Agent",
    // Units and notations the operator reads as one family.
    "RPS",
    "UTC",
    "ms",
    "p50",
    "p95",
    "p99",
    // HTTP status code families.
    "2xx",
    "3xx",
    "4xx",
    "5xx",
    // The third-party sites and products the operator looks up.
    "AbuseIPDB",
    "Cloudflare",
    "DuckDB",
    "GreyNoise",
    "Scamalytics",
    "ipinsights.io",
    "jq",
    // The DNS providers and the Aliyun regions.
    "Aliyun",
    "DNSPod",
    "Tencent",
    "cn-beijing",
    "cn-qingdao",
    "cn-shanghai",
    "cn-shenzhen",
    "us-west-1",
    // The ACME challenge kinds.
    "HTTP-01",
    "dns-01",
    "http-01",
    // The URL schemes a site's backend may use.
    "file",
    "http",
    "https",
    // The configuration keys the log pages quote.
    "api.listen",
    "api_disabled",
    "client_ip",
    "dns_providers.config",
    "domains.dns_provider",
    "log.access_log_capacity",
    "log.access_log_recent",
    "log.bot.capacity",
    "log.bot.dir",
    "log.bot.recent",
    "log.referer.capacity",
    "log.referer.dir",
    "log.referer.recent",
    "log.traffic.enabled",
    // The gateway's own request headers.
    "P-Client-Country",
    "P-Client-Ip",
    "P-Client-Region",
    "P-Known-Bot",
    "X-Client-Country",
    "X-Client-IP",
    "X-Client-Region",
    // The files and the binary the log pages name.
    "bot-.jsonl",
    "bot-YYYY-MM-DD.jsonl",
    "ngx.yml",
    "pangolin-ngx",
    "referer-.jsonl",
    "referer-YYYY-MM-DD.jsonl",
    // The paths the pages quote.
    "/api/v1/",
    "/robots.txt",
    "/sitemap.xml",
    // The enumeration values the configuration stores.
    "global",
    "lenient",
    // The technical terms the pages emphasize.
    "database",
    "peer",
    "token",
    // The examples the forms carry as placeholders.
    "AKID...",
    "LTAI...",
    "example.com",
    "internal.example.com",
    "main-cf",
    "my-site",
    "office-node",
    "/etc/ssl/certs/example.crt",
    "/etc/ssl/private/example.key",
    "file:///var/www/static",
    "http://127.0.0.1:8080",
    // The examples the forms carry whole — the wording is part of the
    // example, so the whole string is listed.
    "&lt;h1&gt;Forbidden&lt;/h1&gt;",
    "&lt;h1&gt;Too many requests&lt;/h1&gt;",
    "203.0.113.7 or 10.0.0.0/8",
    // The configuration snippets the log pages quote as they are.
    "log.traffic.enabled = false",
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
const LONG_TEXT_HANZI: usize = 20;

/// Whether `c` is a Chinese ideograph.
///
/// The block is the one the space gate uses: the full-width
/// punctuation (`。`, `，`, `（`) lives outside it, and it is *correct*
/// to write "。Cloudflare" without a space.
fn is_hanzi(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// Whether `c` belongs to a Chinese, a Japanese or a Korean script.
///
/// Wider than [`is_hanzi`]: the residual-English gate has to exempt a
/// text node that carries *any* CJK character, not only Han ones.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4dbf      // CJK extension A
        | 0x4e00..=0x9fff    // CJK unified ideographs
        | 0xf900..=0xfaff    // CJK compatibility ideographs
        | 0x3040..=0x30ff    // hiragana and katakana
        | 0xac00..=0xd7af    // hangul syllables
    )
}

/// Whether `value` contains at least one Chinese ideograph.
fn has_hanzi(value: &str) -> bool {
    value.chars().any(is_hanzi)
}

/// Whether `value` contains at least one CJK character.
fn has_cjk(value: &str) -> bool {
    value.chars().any(is_cjk)
}

/// Whether `value` holds two ASCII letters next to each other.
///
/// Two is the threshold: a single letter is a variable or a unit
/// ("p95", "5 s"), two are the beginning of a word.
fn has_letter_run(value: &str) -> bool {
    let mut previous = false;
    for c in value.chars() {
        let letter = c.is_ascii_alphabetic();
        if letter && previous {
            return true;
        }
        previous = letter;
    }
    false
}

/// The number of Chinese characters in `value`.
fn hanzi_count(value: &str) -> usize {
    value.chars().filter(|c| is_hanzi(*c)).count()
}

/// Whether `value` is a Chinese text that still carries half-width
/// punctuation.
///
/// A value without a Chinese character is exempt: a technical
/// identifier has no Chinese punctuation to convert.
fn has_half_width_punctuation(value: &str) -> bool {
    has_hanzi(value) && value.chars().any(|c| HALF_WIDTH_PUNCTUATION.contains(&c))
}

/// Whether `value` glues a Chinese character to an ASCII word, a number
/// or a placeholder.
///
/// The ASCII side is "printable, not a space": a Latin word
/// ("DNS记录"), a digit ("共3条") and a placeholder brace ("共{n}条")
/// all read badly without the half-width space Chinese typography
/// puts between a Chinese character and a foreign one.
///
/// A value without a Chinese character is exempt.
fn needs_space(value: &str) -> bool {
    if !has_hanzi(value) {
        return false;
    }
    let chars: Vec<char> = value.chars().collect();
    chars.windows(2).any(|window| {
        let (left, right) = (window[0], window[1]);
        (is_hanzi(left) && right.is_ascii_graphic()) || (left.is_ascii_graphic() && is_hanzi(right))
    })
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
/// The two catalogs carry the same key set — `catalog_completeness`
/// rejects a key that exists in only one of them — so this list walks
/// the whole catalog. It exists to feed the gates that look at the
/// *values*.
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
fn untranslated_keys() -> Vec<&'static str> {
    shared_keys()
        .into_iter()
        .filter(|pair| {
            pair.chinese == pair.english && !IDENTICAL_OK.iter().any(|(key, _)| *key == pair.key)
        })
        .map(|pair| pair.key)
        .collect()
}

/// The keys whose Chinese text contains half-width punctuation.
fn half_width_punctuation_keys() -> Vec<&'static str> {
    shared_keys()
        .into_iter()
        .filter(|pair| has_half_width_punctuation(pair.chinese))
        .map(|pair| pair.key)
        .collect()
}

/// The keys whose Chinese text glues a Chinese character to an ASCII
/// word, a number or a placeholder without a space.
fn missing_space_keys() -> Vec<&'static str> {
    shared_keys()
        .into_iter()
        .filter(|pair| needs_space(pair.chinese))
        .map(|pair| pair.key)
        .collect()
}

/// The keys whose Chinese text holds more than [`LONG_TEXT_HANZI`]
/// Chinese characters, with the count.
fn long_keys() -> Vec<(&'static str, usize)> {
    shared_keys()
        .into_iter()
        .filter_map(|pair| {
            let count = hanzi_count(pair.chinese);
            (count > LONG_TEXT_HANZI).then_some((pair.key, count))
        })
        .collect()
}

/// The end of the region that starts with `open` and ends with
/// `close`, if the region is there at all.
fn skip_region(text: &str, open: &str, close: &str) -> Option<usize> {
    let start = text.strip_prefix(open)?;
    let end = start.find(close)?;
    Some(open.len() + end + close.len())
}

/// The end of a `<script>` or a `<style>` element, body included.
fn skip_element(text: &str) -> Option<usize> {
    let name = if text.starts_with("<script") {
        "script"
    } else {
        "style"
    };
    let open_end = text.find('>')? + 1;
    let close = format!("</{name}");
    let close_at = text[open_end..].find(&close)? + open_end;
    Some(close_at + close.len())
}

/// The newlines of a skipped region.
///
/// The regions are replaced by their newlines so a finding can be
/// reported with the line it is on.
fn newlines(text: &str) -> String {
    "\n".repeat(text.matches('\n').count())
}

/// The template with everything that is not operator-visible text
/// removed: the HTML comments, the askama comments, the script and the
/// style bodies, and the template expressions.
///
/// The template expressions go first: they may hold a `>` or a `<` that
/// would otherwise be read as a tag.
fn strip_non_text(body: &str) -> String {
    const REGIONS: &[(&str, &str)] = &[("<!--", "-->"), ("{#", "#}"), ("{{", "}}"), ("{%", "%}")];
    let mut out = String::with_capacity(body.len());
    let mut index = 0usize;
    while index < body.len() {
        let rest = &body[index..];
        let skipped = REGIONS
            .iter()
            .find_map(|(open, close)| skip_region(rest, open, close));
        if let Some(end) = skipped {
            out.push_str(&newlines(&rest[..end]));
            index += end;
            continue;
        }
        if (rest.starts_with("<script") || rest.starts_with("<style"))
            && let Some(end) = skip_element(rest)
        {
            out.push_str(&newlines(&rest[..end]));
            index += end;
            continue;
        }
        let c = rest.chars().next().expect("the rest is not empty");
        out.push(c);
        index += c.len_utf8();
    }
    out
}

/// The values of the operator-facing attributes, with the offset of
/// each value.
///
/// The four attributes are the ones that carry text the operator
/// reads: the tooltip, the input hint, the accessible name and the
/// confirmation dialog.
fn attribute_values(body: &str) -> Vec<(usize, &str)> {
    const ATTRIBUTES: &[&str] = &["title", "placeholder", "aria-label", "hx-confirm"];
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        // The scan walks the bytes but only ever slices at a character
        // boundary.
        let cursor = index;
        let mut found = None;
        for name in ATTRIBUTES {
            if !body[cursor..].starts_with(name) {
                continue;
            }
            // The name must not be the tail of a longer attribute
            // (`data-title`).
            if cursor > 0
                && (bytes[cursor - 1].is_ascii_alphanumeric()
                    || bytes[cursor - 1] == b'-'
                    || bytes[cursor - 1] == b'_')
            {
                continue;
            }
            let mut after = cursor + name.len();
            while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                after += 1;
            }
            if after >= bytes.len() || bytes[after] != b'=' {
                continue;
            }
            after += 1;
            while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                after += 1;
            }
            // Both quote styles are valid HTML, and a template that
            // uses the other one must not slip past the gate.
            if after >= bytes.len() || (bytes[after] != b'"' && bytes[after] != b'\'') {
                continue;
            }
            found = Some((after + 1, bytes[after] as char));
            break;
        }
        if let Some((start, quote)) = found {
            let Some(rel_end) = body[start..].find(quote) else {
                break;
            };
            let end = start + rel_end;
            out.push((start, &body[start..end]));
            index = end;
            continue;
        }
        // Advance by a whole character.
        let c = body[index..].chars().next().expect("the rest is not empty");
        index += c.len_utf8();
    }
    out
}

/// The text nodes of a template body — the parts that are not inside a
/// tag — with the offset of each node.
fn text_nodes(body: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'<' {
            let node = &body[start..index];
            if !node.trim().is_empty() {
                out.push((start, node));
            }
            index = body[index..]
                .find('>')
                .map_or(body.len(), |at| index + at + 1);
            start = index;
            continue;
        }
        index += 1;
    }
    let node = &body[start..];
    if !node.trim().is_empty() {
        out.push((start, node));
    }
    out
}

/// The text of a node with its whitespace collapsed.
fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a piece of text is English prose the catalog should have
/// provided.
///
/// A text node is English when it holds two ASCII letters next to each
/// other and no CJK character at all: a node that carries a Chinese
/// character is a mixed sentence ("Pangolin — 内网穿透与反向代理"),
/// and that is allowed.
fn is_english_residue(value: &str) -> bool {
    has_letter_run(value) && !has_cjk(value)
}

/// Whether a piece of English may stay in a template.
///
/// A literal is allowed when the whole text is whitelisted, or when
/// every word of it that holds letters is: "— Pangolin" is allowed
/// because "Pangolin" is.
fn is_allowed_english(value: &str) -> bool {
    if ALLOWED_ENGLISH_TEXT.contains(&value) {
        return true;
    }
    value
        .split_whitespace()
        .filter(|word| has_letter_run(word))
        .all(|word| ALLOWED_ENGLISH_TEXT.contains(&word))
}

/// The line an offset is on.
fn line_of(body: &str, offset: usize) -> usize {
    body[..offset].matches('\n').count() + 1
}

/// The English the templates carry instead of asking the catalog for
/// it: the file, the line and the text.
fn template_residue() -> Vec<(String, usize, String)> {
    let mut residue = Vec::new();
    for path in files_with_extension(&crate_root().join("templates"), &["html"]) {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let cleaned = strip_non_text(&body);
        let file = path
            .strip_prefix(crate_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        let mut findings: Vec<(usize, String)> = Vec::new();
        for (offset, value) in attribute_values(&cleaned) {
            findings.push((offset, normalize(value)));
        }
        for (offset, node) in text_nodes(&cleaned) {
            findings.push((offset, normalize(node)));
        }
        for (offset, value) in findings {
            if !is_english_residue(&value) || is_allowed_english(&value) {
                continue;
            }
            residue.push((file.clone(), line_of(&cleaned, offset), value));
        }
    }
    residue
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
            "these Chinese strings glue a Chinese character to a Latin word, a \
             number or a placeholder without a space:\n{}",
            offenders(&keys)
        );
    }

    /// Gate 4 — 残留英文扫描.
    ///
    /// Every operator-facing string comes from the catalog.  A
    /// template that spells English out instead of asking for it is
    /// the residue this gate kills; the literals that are meant to stay
    /// English are listed in `ALLOWED_ENGLISH_TEXT`.
    #[test]
    fn no_template_carries_english_prose() {
        let residue = template_residue();
        let report = residue
            .iter()
            .map(|(file, line, text)| format!("{file}:{line}: {text:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            residue.is_empty(),
            "these templates carry English the catalog should provide — replace \
             it with a `lang.t(…)` reference, or add the literal to \
             ALLOWED_ENGLISH_TEXT:\n{report}"
        );
    }

    /// The predicates on their own.
    ///
    /// The gates above read the shipped catalog, so a broken predicate
    /// would leave them green: the punctuation gate would find
    /// nothing because nothing was classified as Chinese.  These
    /// tests pin the predicates against synthetic values, so a
    /// predicate that stops working turns *this* red.
    #[test]
    fn the_punctuation_predicate_sees_half_width_punctuation() {
        assert!(has_half_width_punctuation("登录管理后台 (test)"));
        assert!(has_half_width_punctuation("错误: connection refused"));
        assert!(!has_half_width_punctuation("登录管理后台（测试）"));
        assert!(!has_half_width_punctuation("DNS 记录"));
        // A pure ASCII value is exempt — it has no Chinese
        // punctuation to convert.
        assert!(!has_half_width_punctuation("DNS"));
        assert!(!has_half_width_punctuation("User-Agent"));
    }

    /// The space predicate on its own — see
    /// `the_punctuation_predicate_sees_half_width_punctuation` for
    /// why the predicates need their own tests.
    #[test]
    fn the_space_predicate_sees_glued_words() {
        // A Latin word.
        assert!(needs_space("DNS记录"));
        // A number.
        assert!(needs_space("共3条"));
        // A placeholder.
        assert!(needs_space("共{n}条"));
        assert!(!needs_space("DNS 记录"));
        assert!(!needs_space("共 {n} 条"));
        // A pure ASCII value is exempt.
        assert!(!needs_space("DNS"));
    }

    /// The residue predicate on its own.
    #[test]
    fn the_residue_predicate_sees_english_without_chinese() {
        assert!(is_english_residue("Save"));
        assert!(is_english_residue("Delete site foo?"));
        // One letter is not a word.
        assert!(!is_english_residue("5 s"));
        // A mixed sentence is not residue.
        assert!(!is_english_residue("Pangolin — 内网穿透与反向代理"));
        assert!(!is_english_residue("保存"));
    }

    /// The whitelist of the literal English.
    #[test]
    fn the_english_whitelist_accepts_whole_and_word_wise() {
        // A whole literal.
        assert!(is_allowed_english("Pangolin"));
        // A word of a longer text.
        assert!(is_allowed_english("— Pangolin"));
        // A whole value that is listed as a whole.
        assert!(is_allowed_english("&lt;h1&gt;Forbidden&lt;/h1&gt;"));
        // Prose is not.
        assert!(!is_allowed_english("Delete site foo?"));
        assert!(!is_allowed_english("Save"));
    }

    /// The scanner on its own — a synthetic template, so the shape of
    /// the scan is pinned independently of what the templates hold.
    #[test]
    fn the_scanner_finds_the_english_in_a_template() {
        let template = "\
<!-- a comment with Delete site foo? -->
<button title=\"Delete site foo?\">Save</button>
<p>{{ lang.t(\"nav.sites\") }}</p>
<script>var x = \"Delete site foo?\";</script>
";
        let cleaned = strip_non_text(template);
        // The comment, the script body and the expression are gone.
        assert!(!cleaned.contains("comment"));
        assert!(!cleaned.contains("var x"));
        assert!(!cleaned.contains("lang.t"));
        // The button's text and its tooltip are there.
        let nodes: Vec<String> = text_nodes(&cleaned)
            .into_iter()
            .map(|(_, node)| normalize(node))
            .collect();
        assert!(nodes.contains(&"Save".to_string()));
        let attributes: Vec<String> = attribute_values(&cleaned)
            .into_iter()
            .map(|(_, value)| normalize(value))
            .collect();
        assert!(attributes.contains(&"Delete site foo?".to_string()));
        // Both are residue.
        assert!(is_english_residue("Save"));
        assert!(is_english_residue("Delete site foo?"));
    }

    /// The attribute scan reads both quote styles.
    ///
    /// HTML allows either, and a template that uses the other one must
    /// not slip past the gate.
    #[test]
    fn the_attribute_scan_reads_both_quote_styles() {
        let template = "\
<button title=\"Delete site foo?\">Save</button>
<button title='Delete site foo?'>Save</button>
<button placeholder='Place holder'>Save</button>
";
        let cleaned = strip_non_text(template);
        let attributes: Vec<String> = attribute_values(&cleaned)
            .into_iter()
            .map(|(_, value)| normalize(value))
            .collect();
        assert_eq!(
            attributes
                .iter()
                .filter(|value| *value == "Delete site foo?")
                .count(),
            2,
            "both quote styles must be read: {attributes:?}"
        );
        assert!(
            attributes.contains(&"Place holder".to_string()),
            "a single-quoted placeholder must be read: {attributes:?}"
        );
    }

    /// The whitelist has to stay honest: an entry that names a key
    /// which is *not* identical any more is a stale entry, and a stale
    /// entry hides the next untranslated string.
    #[test]
    fn the_whitelist_names_only_identical_strings() {
        let chinese = catalog_for(Lang::ZhCn);
        let english = catalog_for(Lang::En);
        for (key, reason) in IDENTICAL_OK {
            assert!(
                !reason.is_empty(),
                "the whitelist names {key} without saying why"
            );
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
        for (key, _) in IDENTICAL_OK {
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
    ///
    /// libtest swallows the output of a *passing* test, so the report
    /// only shows up with `--nocapture`:
    ///
    /// ```text
    /// cargo test -p admin --lib i18n::quality -- --nocapture
    /// ```
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
