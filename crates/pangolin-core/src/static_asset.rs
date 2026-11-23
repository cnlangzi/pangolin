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
/// stack buffer and compared against the list of byte-string
/// literals.
pub fn is_static_asset_target(target: &str) -> bool {
    let path = target.split('?').next().unwrap_or(target);
    let Some(segment) = path.rsplit('/').next() else {
        return false;
    };
    let Some((_, extension)) = segment.rsplit_once('.') else {
        return false;
    };
    if extension.is_empty() || extension.len() > MAX_EXTENSION_LEN {
        return false;
    }
    let mut lower = [0u8; MAX_EXTENSION_LEN];
    for (slot, byte) in lower.iter_mut().zip(extension.bytes()) {
        *slot = byte.to_ascii_lowercase();
    }
    match &lower[..extension.len()] {
        // Images.
        b"png" | b"jpg" | b"jpeg" | b"jfif" | b"gif" | b"bmp" | b"ico" | b"svg" | b"apng"
        | b"webp" | b"avif" | b"tif" | b"tiff" | b"heic" | b"heif" => true,
        // Scripts and source maps.
        b"js" | b"mjs" | b"cjs" | b"map" | b"wasm" => true,
        // Stylesheets.
        b"css" => true,
        // Fonts.
        b"woff" | b"woff2" | b"ttf" | b"otf" | b"eot" => true,
        // Media and playlists.
        b"mp4" | b"webm" | b"m4v" | b"mov" | b"avi" | b"mkv" | b"mp3" | b"m4a" | b"wav"
        | b"ogg" | b"oga" | b"opus" | b"flac" | b"aac" | b"weba" | b"m3u8" | b"mpd" => true,
        // Archives.
        b"zip" | b"gz" | b"tgz" | b"bz2" | b"xz" | b"7z" | b"rar" | b"tar" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_static_asset_target;

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
}
