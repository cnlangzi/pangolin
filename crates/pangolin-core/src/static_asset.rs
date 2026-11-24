//! Static-asset detection.
//!
//! The rate limiter counts *content* requests: a crawler that walks a
//! site's images has fetched nothing, so the limits must not treat it
//! as a content fetcher. This module answers the one question the
//! limiter needs — is this request target a static asset?
//!
//! The answer comes from the **extension of the path**. The response's
//! `Content-Type` would be the better source, but the limiter decides
//! before the backend is contacted, so no response header exists yet.
//! The path is read as it arrived, without percent-decoding.
//!
//! See `docs/design/rate-limit.md`.

/// The longest extension the comparison accepts, in bytes.
///
/// The list's longest extension is five bytes; the cap bounds the
/// stack buffer the comparison runs on, so a longer one is rejected
/// without being copied into it.
const MAX_EXTENSION_LEN: usize = 8;

/// The extensions that count as a static asset.
///
/// **One list, two readers** (issue #259): the rate limiter asks
/// [`is_static_asset_target`] whether a request target is a static
/// asset, and the edge cache asks the same question through
/// `cache.types`, whose default is this list. A second table would
/// drift from this one — the limiter would exempt a file the cache
/// stores, or the other way round — so the cache's default *is* this
/// list.
///
/// The list is **sorted**: [`is_static_asset_target`] binary-searches
/// it, and a search over an unsorted list silently misses. The
/// `the_extension_list_is_sorted` test pins the order.
pub const STATIC_ASSET_EXTENSIONS: &[&str] = &[
    "7z", "aac", "apng", "avi", "avif", "bmp", "bz2", "cjs", "css", "eot", "flac", "gif", "gz",
    "heic", "heif", "ico", "jfif", "jpeg", "jpg", "js", "m3u8", "m4a", "m4v", "map", "mjs", "mkv",
    "mov", "mp3", "mp4", "mpd", "oga", "ogg", "opus", "otf", "png", "rar", "svg", "tar", "tgz",
    "tif", "tiff", "ttf", "wasm", "wav", "weba", "webm", "webp", "woff", "woff2", "xz", "zip",
];

/// Whether the request target is a static asset.
///
/// `target` is the request target as the client sent it — the path
/// plus the query string (`uri.path_and_query()`), not
/// percent-decoded.
///
/// The decision reads the extension of the **last path segment**: the
/// text after its last `.`. The query string takes no part in it, so
/// `/app.css?v=3` is a static asset while `/download?file=x.js` is
/// not. The comparison is case-insensitive.
///
/// A target without an extension, with an empty extension or with an
/// extension longer than [`MAX_EXTENSION_LEN`] is not a static asset.
///
/// The function allocates nothing: the extension is lower-cased into a
/// stack buffer and looked up in [`STATIC_ASSET_EXTENSIONS`] with a
/// binary search.
pub fn is_static_asset_target(target: &str) -> bool {
    lower_extension(target).is_some_and(|extension| {
        STATIC_ASSET_EXTENSIONS
            .binary_search(&extension.as_str())
            .is_ok()
    })
}

/// A lower-cased file extension, held in a fixed-size buffer.
///
/// The buffer is part of the value, so [`LowerExtension::as_str`]
/// borrows from the value and not from a stack frame that is already
/// gone — the extension can be returned to a caller without
/// allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LowerExtension {
    buf: [u8; MAX_EXTENSION_LEN],
    len: usize,
}

impl LowerExtension {
    /// The extension, lower-cased.
    pub fn as_str(&self) -> &str {
        // The buffer only ever holds lower-cased ASCII.
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// The extension of a request target's last path segment, lower-cased.
///
/// `None` when the target has no extension, with an empty one, or with
/// one longer than [`MAX_EXTENSION_LEN`].
///
/// The edge cache asks for the extension itself rather than for the
/// boolean: it compares the extension against the operator's
/// `cache.types` list, which may be wider or narrower than
/// [`STATIC_ASSET_EXTENSIONS`].
pub fn lower_extension(target: &str) -> Option<LowerExtension> {
    let path = match target.split_once('?') {
        Some((path, _query)) => path,
        None => target,
    };
    let segment = match path.rsplit_once('/') {
        Some((_dir, segment)) => segment,
        None => path,
    };
    let (_stem, extension) = segment.rsplit_once('.')?;
    if extension.is_empty() || extension.len() > MAX_EXTENSION_LEN {
        return None;
    }
    let mut buf = [0u8; MAX_EXTENSION_LEN];
    for (slot, byte) in buf.iter_mut().zip(extension.bytes()) {
        *slot = byte.to_ascii_lowercase();
    }
    Some(LowerExtension {
        buf,
        len: extension.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::{STATIC_ASSET_EXTENSIONS, is_static_asset_target, lower_extension};

    #[test]
    fn an_image_extension_is_a_static_asset() {
        assert!(is_static_asset_target("/img/1.png"));
        assert!(is_static_asset_target("/assets/logo.svg"));
        assert!(is_static_asset_target("/favicon.ico"));
    }

    #[test]
    fn every_listed_category_is_a_static_asset() {
        for target in [
            "/a.png", "/a.js", "/a.css", "/a.woff2", "/a.mp4", "/a.m3u8", "/a.zip",
        ] {
            assert!(is_static_asset_target(target), "{target} must be static");
        }
    }

    #[test]
    fn the_extension_is_read_from_the_path_and_not_the_query() {
        assert!(is_static_asset_target("/app.css?v=3"));
        assert!(
            !is_static_asset_target("/download?file=x.js"),
            "an extension in the query string is not the path's extension"
        );
    }

    #[test]
    fn a_dot_in_a_directory_name_is_not_an_extension() {
        assert!(!is_static_asset_target("/v1.2/page"));
    }

    #[test]
    fn the_extension_match_is_case_insensitive() {
        assert!(is_static_asset_target("/logo.PNG"));
        assert!(is_static_asset_target("/app.Css"));
    }

    #[test]
    fn a_content_extension_is_not_a_static_asset() {
        for target in [
            "/index.html",
            "/index.htm",
            "/index.php",
            "/data.json",
            "/data.xml",
            "/notes.txt",
            "/paper.pdf",
            "/readme.md",
        ] {
            assert!(!is_static_asset_target(target), "{target} must be a page");
        }
    }

    #[test]
    fn a_path_without_an_extension_is_not_a_static_asset() {
        assert!(!is_static_asset_target("/media/12345"));
        assert!(!is_static_asset_target("/assets/"));
        assert!(!is_static_asset_target("/"));
    }

    #[test]
    fn an_empty_or_overlong_extension_is_not_a_static_asset() {
        assert!(!is_static_asset_target("/file."));
        assert!(!is_static_asset_target("/file.abcdefghi"));
    }

    /// The binary search over [`STATIC_ASSET_EXTENSIONS`] only finds an
    /// entry when the list is sorted. An unsorted list would make the
    /// predicate silently miss extensions — and the edge cache's
    /// `cache.types` default would stop matching the limiter's
    /// classification.
    #[test]
    fn the_extension_list_is_sorted() {
        let mut sorted = STATIC_ASSET_EXTENSIONS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            sorted, STATIC_ASSET_EXTENSIONS,
            "STATIC_ASSET_EXTENSIONS must stay sorted"
        );
    }

    /// Every extension the list names is a static asset, and the
    /// predicate accepts nothing outside it — the two readers of the
    /// list (the limiter and the cache) cannot drift apart.
    #[test]
    fn every_listed_extension_is_a_static_asset() {
        for extension in STATIC_ASSET_EXTENSIONS {
            let target = format!("/asset.{extension}");
            assert!(
                is_static_asset_target(&target),
                "{target} must be a static asset"
            );
            assert_eq!(
                lower_extension(&target).map(|e| e.as_str().to_string()),
                Some((*extension).to_string()),
                "the extension reader must agree with the predicate"
            );
        }
    }

    #[test]
    fn the_extension_reader_returns_the_lower_cased_extension() {
        assert_eq!(
            lower_extension("/a/Logo.PNG").map(|e| e.as_str().to_string()),
            Some("png".to_string())
        );
        assert_eq!(
            lower_extension("/app.css?v=3").map(|e| e.as_str().to_string()),
            Some("css".to_string())
        );
        assert!(lower_extension("/download?file=x.js").is_none());
        assert!(lower_extension("/v1.2/page").is_none());
        assert!(lower_extension("/file.").is_none());
    }
}
