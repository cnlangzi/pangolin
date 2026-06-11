//! Admin UI asset pipeline (issue #32).
//!
//! Migrates from hand-rolled `DefaultHasher` + `build.rs` + `include_str!`
//! to [`rust-embed`] + [`sha2`]. The `Asset` struct snapshots every file in
//! `crates/admin/../assets/` at compile time (release) or reads from the
//! filesystem at runtime (`debug-embed` feature in dev builds).
//!
//! ## Hashing
//!
//! Both code paths produce an identical SHA-256 hash because the bytes
//! fed to the hasher are the same (compile-time snapshotted vs runtime
//! filesystem-read). The first 12 hex chars (48 bits) are exposed as
//! `CSS_HASH` / `JS_HASH`. The URL uses `?v=<12hex>` and the response
//! carries `Cache-Control: public, max-age=31536000, immutable`, so a
//! hash change = a new URL = guaranteed cache miss; no 304 round-trip.
//!
//! ## `PANGOLIN_ADMIN_JS=raw`
//!
//! Setting the env var to `raw` selects `app.js` (unminified, dev/debug)
//! at startup; any other value (or unset) defaults to `app.min.js`
//! (esbuild `--minify` output, production). The selected filename is
//! captured in [`JS_FILE`] once at startup and used by both the
//! `__JS_FILE__` template placeholder and the `/admin/app.min.js`
//! route handler.

use std::sync::LazyLock;

use rust_embed::Embed;
use sha2::{Digest, Sha256};

/// Snapshot of every file in `crates/admin/../assets/` (i.e. the workspace
/// `assets/` directory).
///
/// - **Release build**: bytes are embedded into the binary, so the
///   release binary serves assets with the working directory's `assets/`
///   absent.
/// - **Debug build** (`debug-embed` feature): `Asset::get()` reads from
///   the filesystem at runtime, so editors and `make build-ui` take
///   effect without recompiling.
#[derive(Embed)]
#[folder = "../../assets/"]
pub struct Asset;

/// The active JS bundle filename. Selected at process startup from
/// `PANGOLIN_ADMIN_JS` (`raw` → `app.js`, anything else / unset →
/// `app.min.js`). Used by the `__JS_FILE__` template placeholder and by
/// the `/admin/app.js` vs `/admin/app.min.js` route dispatch.
pub static JS_FILE: LazyLock<&'static str> = LazyLock::new(|| {
    let raw = std::env::var("PANGOLIN_ADMIN_JS").ok().as_deref() == Some("raw");
    let chosen: &'static str = if raw { "app.js" } else { "app.min.js" };
    log::info!(
        "admin assets: PANGOLIN_ADMIN_JS={:?} → serving {}",
        std::env::var("PANGOLIN_ADMIN_JS").ok(),
        chosen,
    );
    chosen
});

/// Number of leading hex chars of the SHA-256 digest used as cache-bust
/// token.48 bits is enough to make accidental collisions effectively
/// impossible for a small admin bundle while keeping URLs short.
const SHORT_HASH_LEN: usize = 12;

/// CSS bundle content hash, computed from `app.css`. First 12 hex chars
/// of the SHA-256 digest (48-bit).
pub static CSS_HASH: LazyLock<String> = LazyLock::new(|| short_hash("app.css"));

/// JS bundle content hash. Computed from whichever file `JS_FILE` points
/// at (`app.min.js` in production, `app.js` in raw mode), so the URL's
/// `?v=` token always matches the bytes actually served.
pub static JS_HASH: LazyLock<String> = LazyLock::new(|| short_hash(&JS_FILE));

/// Bytes of the CSS bundle. Returns the rust-embed snapshot (embedded
/// in release, fs-read in dev). Empty Vec on miss; the caller still
/// serves the response (a missing CSS will render but unstyled, which
/// is loud enough to surface the broken build).
pub fn css_bytes() -> Vec<u8> {
    <Asset as rust_embed::RustEmbed>::get("app.css")
        .map(|f| f.data.into_owned())
        .unwrap_or_default()
}

/// Bytes of the active JS bundle (`app.js` or `app.min.js` per `JS_FILE`).
pub fn js_bytes() -> Vec<u8> {
    <Asset as rust_embed::RustEmbed>::get(&JS_FILE)
        .map(|f| f.data.into_owned())
        .unwrap_or_default()
}

/// MIME type for the CSS response, per issue spec.
pub const CSS_MIME: &str = "text/css; charset=utf-8";
/// MIME type for the JS response, per issue spec.
pub const JS_MIME: &str = "application/javascript; charset=utf-8";
/// `Cache-Control` value for fingerprinted, content-addressed assets.
/// Paired with the `?v=<hash>` URL, this gives operators the standard
/// immutable-cache behavior: hash change → new URL → guaranteed miss.
pub const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// Compute the SHA-256 digest of the file's bytes and return the first
/// `SHORT_HASH_LEN` hex chars. We hash via [`sha2`] (rather than
/// `Metadata::sha256_hash()`) so the digest is computed over the same
/// bytes the server will hand the client — rust-embed's metadata is
/// stable across release/dev, but hashing via `sha2` keeps the data
/// path obvious (one crate owns both hashing and serving). Either
/// approach gives an identical 32-byte digest for identical input.
fn short_hash(name: &str) -> String {
    match <Asset as rust_embed::RustEmbed>::get(name) {
        Some(file) => {
            let mut hasher = Sha256::new();
            hasher.update(&file.data);
            let digest = hasher.finalize();
            hex_prefix(digest.as_slice(), SHORT_HASH_LEN)
        }
        None => {
            log::warn!(
                "admin assets: no embedded file for {}, hash will be empty",
                name
            );
            String::new()
        }
    }
}

/// First `len` hex chars of a SHA-256 digest (up to 64).
fn hex_prefix(digest: &[u8], len: usize) -> String {
    let mut s = String::with_capacity(len);
    for byte in digest.iter().take(len.div_ceil(2)) {
        let hi = (byte >> 4) & 0x0f;
        let lo = byte & 0x0f;
        if s.len() < len {
            s.push(std::char::from_digit(hi.into(), 16).unwrap());
        }
        if s.len() < len {
            s.push(std::char::from_digit(lo.into(), 16).unwrap());
        }
    }
    s
}
