//! Admin UI — interface language.
//!
//! The admin panel is server-rendered: askama templates + htmx. The
//! language is therefore resolved **on the server**, per request, and
//! handed down to the templates as a [`Lang`] value — there is no
//! client-side dictionary to apply after an htmx swap, so a partial
//! refresh never flashes the wrong language.
//!
//! ## The pieces
//!
//! - [`Lang`] — the language enum (`En`, `ZhCn`).
//! - [`catalog`] — the per-language JSON files, embedded at compile
//!   time and parsed once per process.
//! - [`Lang::t`] / [`Lang::tf`] — the lookup and the placeholder
//!   substitution.
//! - [`parse_accept_language`] — the `Accept-Language` negotiation.
//! - [`parse_lang_cookie`] / [`make_lang_cookie`] — the cookie that
//!   remembers the operator's choice.
//! - [`sanitize_next`] — the guard on the `next` parameter of the
//!   language switcher (an open-redirect guard).
//!
//! ## The fallback chain
//!
//! A key that the target language does not have falls back to the
//! English catalog, and finally to a visible `[missing:key]` marker
//! plus one `log::error!` line. Nothing in this module panics: a
//! panic inside a request handler becomes a 500.

pub mod catalog;

use std::borrow::Cow;

pub use catalog::{AREAS, COMPLETED_AREAS, Catalog, catalog_for};

/// Name of the cookie that carries the operator's chosen language.
///
/// The cookie is written by the server and read by the server, so it
/// is `HttpOnly` like every other admin cookie.
pub const LANG_COOKIE: &str = "pangolin_lang";

/// One year, in seconds — how long the language cookie lives.
const LANG_COOKIE_MAX_AGE: u32 = 31_536_000;

/// The interface languages the admin UI knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    En,
    ZhCn,
}

impl Lang {
    /// Every language the admin UI knows about, in the order the
    /// switcher lists them.
    pub const ALL: [Lang; 2] = [Lang::En, Lang::ZhCn];

    /// The BCP-47 tag of the language.
    ///
    /// Doubles as the cookie value and as the `<html lang>` attribute.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::ZhCn => "zh-CN",
        }
    }

    /// The name of the language **in the language itself** — the
    /// switcher lists languages by their native name so an operator
    /// who cannot read the current one can still find theirs.
    pub fn display_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::ZhCn => "简体中文",
        }
    }

    /// Resolve a language tag to a language.
    ///
    /// Accepts the canonical codes and the tags browsers actually
    /// send: `en`, `en-US`, `en-GB`, `zh`, `zh-CN`, `zh-Hans`. The
    /// comparison is case-insensitive (BCP-47 tags are). Anything
    /// else is `None` — the caller falls back to the default.
    pub fn from_code(code: &str) -> Option<Lang> {
        match code.trim().to_ascii_lowercase().as_str() {
            "en" | "en-us" | "en-gb" => Some(Lang::En),
            "zh" | "zh-cn" | "zh-hans" => Some(Lang::ZhCn),
            _ => None,
        }
    }

    /// The languages that actually have text.
    ///
    /// A catalog with no keys is a language the operator cannot pick,
    /// so the switcher only lists the ones that are translated (at
    /// least partly).
    pub fn available() -> Vec<Lang> {
        Lang::ALL
            .iter()
            .copied()
            .filter(|lang| !catalog_for(*lang).is_empty())
            .collect()
    }

    /// The languages the switcher offers.
    ///
    /// The same list as [`Lang::available`], as a method so a template
    /// can ask its own `lang` field.
    pub fn available_langs(self) -> Vec<Lang> {
        Lang::available()
    }

    /// The text of `key` in this language.
    ///
    /// Falls back to the English catalog and finally to a visible
    /// `[missing:key]` marker, so a translation gap never blanks the
    /// interface — and never panics.
    pub fn t(self, key: &str) -> Cow<'static, str> {
        if let Some(text) = catalog_for(self).get(key) {
            return Cow::Borrowed(text.as_str());
        }
        if self != Lang::En
            && let Some(text) = catalog_for(Lang::En).get(key)
        {
            return Cow::Borrowed(text.as_str());
        }
        log::error!("i18n: missing key {key} for language {}", self.code());
        Cow::Owned(format!("[missing:{key}]"))
    }

    /// The text of `key` in this language with the named placeholders
    /// filled in.
    ///
    /// `{name}` in the text is replaced by the value of the `name`
    /// argument. A placeholder without an argument keeps its literal
    /// text: a translation that drops a placeholder must not blank the
    /// sentence around it.
    ///
    /// The arguments are an iterator of `(name, value)` pairs rather
    /// than a `&[(&str, &str)]` slice for two reasons. A slice cannot
    /// be spelled in an askama template — the template grammar has no
    /// `&` operator, so a template can only write an array literal,
    /// and an array does not coerce to a slice in a call position.
    /// And a value is not always a string: a sentence like "Last {n}
    /// events" has to interpolate a number.
    pub fn tf<P>(self, key: &str, args: impl IntoIterator<Item = P>) -> String
    where
        P: Placeholder,
    {
        let owned = self.t(key);
        let text: &str = &owned;
        let args: Vec<(String, String)> = args
            .into_iter()
            .map(|pair| (pair.name().to_string(), pair.value()))
            .collect();
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let tail = &rest[open..];
            let Some(close) = tail.find('}') else {
                // Unbalanced brace — copy the remainder verbatim.
                out.push_str(tail);
                return out;
            };
            let name = &tail[1..close];
            match args.iter().find(|(arg, _)| arg == name) {
                Some((_, value)) => out.push_str(value),
                None => out.push_str(&tail[..=close]),
            }
            rest = &tail[close + 1..];
        }
        out.push_str(rest);
        out
    }

    /// The dictionary the client-side script reads.
    ///
    /// Rendered into the page as `window.__PANGOLIN_I18N` so
    /// `assets/app.js` can speak the operator's language in the
    /// places the server does not render: the login form's live
    /// validation and the backend URL preview.
    ///
    /// The keys are the catalog keys, spelled out one by one so the
    /// catalog gate test sees them referenced.
    pub fn js_dict(self) -> String {
        let entries: [(&str, Cow<'static, str>); 9] = [
            ("js.credentials_verified", self.t("js.credentials_verified")),
            ("js.request_failed", self.t("js.request_failed")),
            ("js.empty", self.t("js.empty")),
            ("js.backend_required", self.t("js.backend_required")),
            ("js.status.connecting", self.t("js.status.connecting")),
            ("js.status.replaying", self.t("js.status.replaying")),
            ("js.status.live", self.t("js.status.live")),
            ("js.status.disconnected", self.t("js.status.disconnected")),
            ("js.logs.backend", self.t("js.logs.backend")),
        ];
        let mut out = String::from("{");
        for (index, (key, value)) in entries.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(key);
            out.push_str("\":");
            out.push_str(&json_string(value));
        }
        out.push('}');
        out
    }
}

/// A value that can be substituted into a catalog placeholder.
///
/// Implemented for the string types and for the integer types — a
/// sentence like "Last {n} events" interpolates a number, and the
/// catalog value stays a plain string either way.
pub trait PlaceholderValue {
    /// The value as it will appear in the rendered text.
    fn render(&self) -> String;
}

impl PlaceholderValue for str {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl PlaceholderValue for String {
    fn render(&self) -> String {
        self.clone()
    }
}

/// Every integer type, so a counter can be interpolated without a
/// conversion at the call site.
macro_rules! placeholder_value_for_int {
    ($($ty:ty),*) => {
        $(impl PlaceholderValue for $ty {
            fn render(&self) -> String {
                self.to_string()
            }
        })*
    };
}

placeholder_value_for_int!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128);

/// A borrowed value renders like the value itself — a template hands
/// `&u64` to a placeholder as often as `u64`.
impl<T: PlaceholderValue + ?Sized> PlaceholderValue for &T {
    fn render(&self) -> String {
        (**self).render()
    }
}

/// A `(name, value)` pair for [`Lang::tf`].
///
/// The name is the part between the braces in the catalog value;
/// the value is what replaces it.
pub trait Placeholder {
    /// The placeholder's name.
    fn name(&self) -> &str;
    /// The value to substitute.
    fn value(&self) -> String;
}

impl<K: AsRef<str>, V: PlaceholderValue> Placeholder for (K, V) {
    fn name(&self) -> &str {
        self.0.as_ref()
    }
    fn value(&self) -> String {
        self.1.render()
    }
}

impl<K: AsRef<str>, V: PlaceholderValue> Placeholder for &(K, V) {
    fn name(&self) -> &str {
        self.0.as_ref()
    }
    fn value(&self) -> String {
        self.1.render()
    }
}

/// Render `value` as a JSON string literal, safe to inline in a
/// `<script>` block.
///
/// `<`, `>` and `&` become `\uXXXX` escapes: a `</script>` inside a
/// catalog value would otherwise close the script element and turn
/// the rest of the value into markup.
fn json_string(value: &str) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    encoded
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// Build the `Set-Cookie` value that stores the operator's choice.
pub fn make_lang_cookie(lang: Lang) -> String {
    format!(
        "{LANG_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={LANG_COOKIE_MAX_AGE}",
        lang.code()
    )
}

/// Read the language cookie out of a `Cookie` header.
///
/// A cookie that holds an unknown value is ignored (not an error):
/// the caller then negotiates from `Accept-Language` and finally
/// falls back to the default language.
pub fn parse_lang_cookie(cookie_header: Option<&str>) -> Option<Lang> {
    let header = cookie_header?;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once('=')
            && key.trim() == LANG_COOKIE
        {
            return Lang::from_code(value);
        }
    }
    None
}

/// Pick the best supported language from an `Accept-Language` header.
///
/// The tags are ordered by their q-weight (highest first); tags with
/// the same weight keep the order the browser sent them in. The first
/// tag that maps to a supported language wins. `None` means "no
/// acceptable language" — the caller uses the default.
pub fn parse_accept_language(header: Option<&str>) -> Option<Lang> {
    let header = header?;
    let mut candidates: Vec<(f32, usize, &str)> = Vec::new();
    for (index, part) in header.split(',').enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (tag, quality) = match part.split_once(';') {
            Some((tag, params)) => {
                let quality = params
                    .split(';')
                    .filter_map(|param| {
                        let param = param.trim();
                        param
                            .strip_prefix("q=")
                            .and_then(|value| value.parse::<f32>().ok())
                    })
                    .next()
                    .unwrap_or(1.0);
                (tag.trim(), quality)
            }
            None => (part, 1.0),
        };
        // `q=0` means "not acceptable" — skip it.
        if quality <= 0.0 {
            continue;
        }
        candidates.push((quality, index, tag));
    }
    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    candidates
        .iter()
        .find_map(|(_, _, tag)| Lang::from_code(tag))
}

/// Sanitise the `next` parameter of the language switcher.
///
/// Only same-site absolute paths are allowed. The value must start
/// with a single `/`: `//host` and `/\host` are both resolved by a
/// browser as a scheme-relative URL to **another** host, which is the
/// open redirect this guard exists to stop. Control characters are
/// rejected because the value ends up in a `Location` header.
pub fn sanitize_next(next: Option<&str>) -> &str {
    let Some(next) = next else {
        return "/";
    };
    if next.is_empty() || !next.starts_with('/') {
        return "/";
    }
    if next.starts_with("//") || next.starts_with("/\\") {
        return "/";
    }
    if next
        .chars()
        .any(|c| c.is_control() || !c.is_ascii_graphic() && c != ' ')
    {
        return "/";
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
    }

    #[test]
    fn from_code_accepts_browser_tags() {
        assert_eq!(Lang::from_code("zh"), Some(Lang::ZhCn));
        assert_eq!(Lang::from_code("zh-CN"), Some(Lang::ZhCn));
        assert_eq!(Lang::from_code("zh-Hans"), Some(Lang::ZhCn));
        assert_eq!(Lang::from_code("zh-cn"), Some(Lang::ZhCn));
        assert_eq!(Lang::from_code("en"), Some(Lang::En));
        assert_eq!(Lang::from_code("en-US"), Some(Lang::En));
        assert_eq!(Lang::from_code("xx"), None);
        assert_eq!(Lang::from_code(""), None);
    }

    #[test]
    fn display_names_are_native() {
        assert_eq!(Lang::En.display_name(), "English");
        assert_eq!(Lang::ZhCn.display_name(), "简体中文");
    }

    #[test]
    fn t_falls_back_to_en_then_to_missing_marker() {
        // A key that only the English catalog has falls back to
        // English for every other language.
        let key = catalog_for(Lang::En)
            .keys()
            .next()
            .expect("en catalog is not empty");
        assert_eq!(
            Lang::ZhCn.t(key).as_ref(),
            catalog_for(Lang::En).get(key).unwrap()
        );
        // A key nobody has is a visible marker, not a panic. The key
        // is built at runtime so the catalog gate test does not read
        // it as a reference to a key that does not exist.
        let missing = format!("no.such.{}", "key");
        assert_eq!(Lang::En.t(&missing), format!("[missing:{missing}]"));
        assert_eq!(Lang::ZhCn.t(&missing), format!("[missing:{missing}]"));
    }

    #[test]
    fn tf_substitutes_named_placeholders() {
        // Normal substitution.
        assert_eq!(Lang::En.tf("time.seconds_ago", [("n", 5)]), "5s ago");
        // Several placeholders in one text.
        assert_eq!(
            Lang::En.tf("bots.paths_showing", [("n", 3), ("total", 12)]),
            "Showing 3 of 12 paths, by hit count."
        );
        // A placeholder without an argument keeps its literal text.
        assert_eq!(
            Lang::En.tf("bots.paths_showing", [("n", 3)]),
            "Showing 3 of {total} paths, by hit count."
        );
    }

    #[test]
    fn lang_cookie_roundtrip() {
        let cookie = make_lang_cookie(Lang::ZhCn);
        assert_eq!(
            cookie,
            "pangolin_lang=zh-CN; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000"
        );
        assert_eq!(parse_lang_cookie(Some(&cookie)), Some(Lang::ZhCn));
        assert_eq!(
            parse_lang_cookie(Some("pangolin_session=abc; pangolin_lang=en")),
            Some(Lang::En)
        );
        // A cookie with an unknown value is ignored, not fatal.
        assert_eq!(parse_lang_cookie(Some("pangolin_lang=xx")), None);
        assert_eq!(parse_lang_cookie(None), None);
        assert_eq!(parse_lang_cookie(Some("other=1")), None);
    }

    #[test]
    fn accept_language_picks_best_supported_tag() {
        assert_eq!(
            parse_accept_language(Some("zh-CN,zh;q=0.9,en;q=0.8")),
            Some(Lang::ZhCn)
        );
        assert_eq!(
            parse_accept_language(Some("en;q=0.5,zh-CN;q=0.9")),
            Some(Lang::ZhCn)
        );
        // Neither tag is supported.
        assert_eq!(parse_accept_language(Some("fr-FR,de")), None);
        // No header at all.
        assert_eq!(parse_accept_language(None), None);
        assert_eq!(parse_accept_language(Some("")), None);
    }

    #[test]
    fn next_sanitizer_rejects_external_urls() {
        assert_eq!(
            sanitize_next(Some("/sites/edit?name=a")),
            "/sites/edit?name=a"
        );
        assert_eq!(sanitize_next(Some("/")), "/");
        assert_eq!(sanitize_next(Some("https://evil.com")), "/");
        assert_eq!(sanitize_next(Some("//evil.com")), "/");
        assert_eq!(sanitize_next(Some("/\\evil.com")), "/");
        assert_eq!(sanitize_next(Some("evil.com")), "/");
        assert_eq!(sanitize_next(Some("/a\nb")), "/");
        assert_eq!(sanitize_next(None), "/");
        assert_eq!(sanitize_next(Some("")), "/");
    }

    #[test]
    fn js_dict_is_valid_json() {
        let dict = Lang::En.js_dict();
        assert!(dict.starts_with('{'));
        let parsed: serde_json::Value = serde_json::from_str(&dict).expect("js dict is valid JSON");
        let obj = parsed.as_object().expect("js dict is an object");
        assert!(obj.contains_key("js.credentials_verified"));
        assert!(obj.contains_key("js.request_failed"));
        assert!(obj.contains_key("js.empty"));
        assert!(obj.contains_key("js.backend_required"));
    }

    #[test]
    fn js_dict_escapes_script_terminators() {
        // A value that could close the <script> element is escaped.
        let escaped = json_string("</script><b>");
        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
        assert!(!escaped.contains('&'));
    }
}

/// The catalog gate tests.
///
/// They scan the templates and the Rust sources for the keys the code
/// references and compare the two sets. A key that is referenced but
/// missing from the catalog renders as `[missing:key]`; a key that is
/// in the catalog but never referenced is dead weight a translator
/// would have to maintain for nothing.
#[cfg(test)]
mod gate {
    use super::Lang;
    use super::catalog::{AREAS, COMPLETED_AREAS, area_of, catalog_for, is_valid_key};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// The root of the admin crate.
    fn crate_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Every file under `dir` whose extension is in `extensions`.
    fn files_with_extension(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_with_extension(&path, extensions));
            } else if extensions.iter().any(|extension| {
                path.extension()
                    .is_some_and(|e| e == std::ffi::OsStr::new(extension))
            }) {
                out.push(path);
            }
        }
        out
    }

    /// Whether `c` can be part of an identifier.
    fn is_ident(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }

    /// The keys the code references.
    ///
    /// A reference is a call to `t("…")` or `tf("…")` with a string
    /// argument. The scan is hand-written rather than a regular
    /// expression: the admin crate does not depend on a regex engine,
    /// and the shape of the call is simple enough to read by hand.
    fn referenced_keys() -> BTreeSet<String> {
        let root = crate_root();
        let mut keys = BTreeSet::new();
        let mut sources = files_with_extension(&root.join("templates"), &["html"]);
        sources.extend(files_with_extension(&root.join("src"), &["rs"]));
        for path in sources {
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            // The whole file is scanned, not line by line: a call may
            // wrap, with the key on the next line.
            {
                let line = body.as_str();
                let bytes = line.as_bytes();
                for index in 0..bytes.len() {
                    if bytes[index] != b't' {
                        continue;
                    }
                    if index > 0 && is_ident(bytes[index - 1]) {
                        continue;
                    }
                    let mut cursor = index + 1;
                    if cursor < bytes.len() && bytes[cursor] == b'f' {
                        cursor += 1;
                    }
                    if cursor >= bytes.len() || bytes[cursor] != b'(' {
                        continue;
                    }
                    cursor += 1;
                    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                        cursor += 1;
                    }
                    if cursor >= bytes.len() || bytes[cursor] != b'"' {
                        continue;
                    }
                    let rest = &line[cursor + 1..];
                    let Some(end) = rest.find('"') else {
                        continue;
                    };
                    let key = &rest[..end];
                    if is_valid_key(key) {
                        keys.insert(key.to_string());
                    }
                }
            }
        }
        keys
    }

    #[test]
    fn catalog_keys_match_code_references() {
        let catalog = catalog_for(Lang::En);
        let referenced = referenced_keys();

        let missing: Vec<&String> = referenced
            .iter()
            .filter(|key| !catalog.contains_key(*key))
            .collect();
        assert!(
            missing.is_empty(),
            "the code references keys that are not in the catalog: {missing:?}"
        );

        let unused: Vec<&String> = catalog
            .keys()
            .filter(|key| !referenced.contains(*key))
            .collect();
        assert!(
            unused.is_empty(),
            "the catalog has keys that nothing references: {unused:?}"
        );
    }

    #[test]
    fn catalog_completeness() {
        let english = catalog_for(Lang::En);
        let chinese = catalog_for(Lang::ZhCn);

        // A translation may not invent a key the English catalog does
        // not have — it would never be rendered.
        let extra: Vec<&String> = chinese
            .keys()
            .filter(|key| !english.contains_key(*key))
            .collect();
        assert!(
            extra.is_empty(),
            "the zh-CN catalog has keys the English catalog does not: {extra:?}"
        );

        // An area that is declared complete has to be complete: the
        // same keys, in both catalogs.
        for area in COMPLETED_AREAS {
            let english_keys: BTreeSet<&String> =
                english.keys().filter(|key| area_of(key) == *area).collect();
            let chinese_keys: BTreeSet<&String> =
                chinese.keys().filter(|key| area_of(key) == *area).collect();
            assert_eq!(
                english_keys, chinese_keys,
                "the area {area} is declared complete but the catalogs disagree"
            );
        }
    }

    #[test]
    fn every_catalog_key_is_well_formed() {
        for (lang, catalog) in [
            (Lang::En, catalog_for(Lang::En)),
            (Lang::ZhCn, catalog_for(Lang::ZhCn)),
        ] {
            for key in catalog.keys() {
                assert!(
                    is_valid_key(key),
                    "the {} catalog has a malformed key: {key}",
                    lang.code()
                );
                assert!(
                    AREAS.contains(&area_of(key)),
                    "the {} catalog has a key in an undeclared area: {key}",
                    lang.code()
                );
            }
        }
    }
}
