//! Admin UI asset pipeline.
//!
//! Assets in the repo-root `assets/` directory are snapshotted by
//! [`rust-embed`] at compile time (release) or read from the filesystem at
//! runtime (`debug-embed` feature, dev builds).
//!
//! ## Build pipeline
//!
//! Single source of truth lives in [`assets/`] at the repo root:
//!
//! - `tailwindcss.css` (tracked) — Tailwind CLI input with `@tailwind` directives
//! - `app.js`           (tracked) — hand-written JS source, no imports
//! - `app.css`          (gitignored) — Tailwind CLI output (unminified in dev,
//!                      minified when produced by `make build-ui-prod` /
//!                      the Docker `builder` stage)
//! - `app.min.js`       (gitignored) — esbuild output, only produced by
//!                      `make build-ui-prod` / the Docker `builder` stage
//!
//! Dev (`make build-ui`) runs Tailwind without `--minify` and skips esbuild
//! entirely — `app.js` has no imports so the raw source is browser-ready.
//! Production (`make build-dist`) re-runs both with `--minify`.
//!
//! [`assets/`]: https://github.com/yaitoo/pangolin/tree/main/assets
//!
//! ## Hashing
//!
//! Hashes are computed once at startup from the served bytes, so the
//! `?v=<hash>` URL stays in lockstep with the response body — a hash change
//! produces a new URL and a guaranteed browser-cache miss; no 304 round-trip.
//!
//! ## `PANGOLIN_ADMIN_JS`
//!
//! `raw` → `app.js`, anything else → `app.min.js`. The choice is captured
//! once at startup in [`JS_FILE`]. If the chosen file is missing,
//! [`js_bytes`] falls back to `app.js` (with a warning) so dev builds that
//! haven't run `make build-ui-prod` still serve the source bundle.

use std::sync::LazyLock;

use rust_embed::Embed;
use sha2::{Digest, Sha256};

/// Snapshot of every file in the repo-root `assets/` directory.
///
/// `#[folder]` is resolved relative to `CARGO_MANIFEST_DIR`
/// (`crates/admin/`), so `../../assets/` lands at the repo root — the single
/// source of truth shared by `make build-ui`, `make build-ui-prod`, and the
/// Docker `builder` stage.
///
/// - **Release**: bytes are embedded into the binary.
/// - **Debug** (`debug-embed`): `Asset::get()` reads from the filesystem at
///   runtime, so `make build-ui` takes effect on the next process restart.
#[derive(Embed)]
#[folder = "../../assets/"]
pub struct Asset;

/// Active JS bundle filename, selected from `PANGOLIN_ADMIN_JS` at startup.
pub static JS_FILE: LazyLock<&'static str> = LazyLock::new(|| {
    let env_val = std::env::var("PANGOLIN_ADMIN_JS").ok();
    let chosen: &'static str = if env_val.as_deref() == Some("raw") {
        "app.js"
    } else {
        "app.min.js"
    };
    log::info!(
        "admin assets: PANGOLIN_ADMIN_JS={:?} → serving {}",
        env_val,
        chosen,
    );
    chosen
});

/// Length (in hex chars) of the cache-bust token. 48 bits is enough that
/// accidental collisions are negligible for a small admin bundle.
const SHORT_HASH_LEN: usize = 12;

/// CSS bundle content hash, computed from `app.css`. First 12 hex chars of
/// the SHA-256 digest.
pub static CSS_HASH: LazyLock<String> = LazyLock::new(|| short_hash("app.css"));

/// JS bundle content hash, computed from whichever file [`JS_FILE`] points at.
pub static JS_HASH: LazyLock<String> = LazyLock::new(|| short_hash(&JS_FILE));

/// Bytes of the CSS bundle. Empty `Vec` on miss; the unstyled page surfaces
/// a broken build loudly enough.
pub fn css_bytes() -> Vec<u8> {
    <Asset as rust_embed::RustEmbed>::get("app.css")
        .map(|f| f.data.into_owned())
        .unwrap_or_default()
}

/// Bytes of the active JS bundle (`app.js` or `app.min.js`, per [`JS_FILE`]).
///
/// If [`JS_FILE`] is `app.min.js` but that file is missing (typical for dev
/// builds that haven't run `make build-ui-prod`), transparently fall back to
/// the unminified `app.js` source. This means dev works out-of-the-box even
/// without setting `PANGOLIN_ADMIN_JS=raw`; the warning log line makes the
/// fallback visible so it doesn't silently mask a broken build-dist.
pub fn js_bytes() -> Vec<u8> {
    if let Some(f) = <Asset as rust_embed::RustEmbed>::get(&JS_FILE) {
        return f.data.into_owned();
    }
    if *JS_FILE != "app.js" {
        if let Some(f) = <Asset as rust_embed::RustEmbed>::get("app.js") {
            log::warn!(
                "admin assets: {} missing, falling back to unminified app.js",
                *JS_FILE
            );
            return f.data.into_owned();
        }
    }
    log::warn!(
        "admin assets: no JS bundle found (tried {}, app.js)",
        *JS_FILE
    );
    Vec::new()
}

pub const CSS_MIME: &str = "text/css; charset=utf-8";
pub const JS_MIME: &str = "application/javascript; charset=utf-8";
/// `Cache-Control` for fingerprinted assets. Paired with `?v=<hash>` URLs,
/// the "hash change → new URL" contract makes one-year immutable caching safe.
pub const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// Compute the SHA-256 digest of the embedded file and return the first
/// [`SHORT_HASH_LEN`] hex chars.
fn short_hash(name: &str) -> String {
    let Some(file) = <Asset as rust_embed::RustEmbed>::get(name) else {
        log::warn!(
            "admin assets: no embedded file for {}, hash will be empty",
            name
        );
        return String::new();
    };
    let digest = Sha256::digest(&file.data);
    hex::encode(&digest[..SHORT_HASH_LEN / 2])
}
