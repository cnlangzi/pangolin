use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Computes short content hashes for the CSS and JS bundles at compile time.
/// Outputs:
///   - cargo:rustc-env=APP_CSS_HASH=<8 chars>
///   - cargo:rustc-env=APP_JS_HASH=<8 chars>
///   - $OUT_DIR/css_hash.rs and $OUT_DIR/js_hash.rs with `pub const …` definitions
///
/// The hashes are used as `?v=<hash>` query strings on `/admin/app.css` and
/// `/admin/app.js` so browsers re-fetch after a rebuild.
fn main() {
    compute_and_emit(
        "app.css",
        "CSS",
        &["assets/app.css", "../assets/app.css", "../../assets/app.css", "../../../assets/app.css"],
        // Also watch the source tailwind file so rebuilding CSS triggers a rebuild.
        &["assets/tailwindcss.css", "../assets/tailwindcss.css"],
    );

    compute_and_emit(
        "app.js",
        "JS",
        &["assets/app.js", "../assets/app.js", "../../assets/app.js", "../../../assets/app.js"],
        // Vendored htmx is bundled by app.js; rebuild when it changes too.
        &["assets/vendor/htmx-1.9.0.min.js", "../assets/vendor/htmx-1.9.0.min.js"],
    );
}

fn compute_and_emit(
    file_name: &str,
    label: &str,
    candidates: &[&str],
    extra_watch: &[&str],
) {
    let path = candidates
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "could not locate {} — checked several relative paths",
                file_name
            )
        });

    println!("cargo:rerun-if-changed={}", path.display());
    for w in extra_watch {
        if Path::new(w).exists() {
            println!("cargo:rerun-if-changed={}", w);
        }
    }

    let bytes = fs::read(path).unwrap_or_else(|e| panic!("failed to read {}: {}", file_name, e));
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    let full = format!("{:x}", hasher.finish());
    let short8 = full[..8].to_string();

    println!("cargo:rustc-env=APP_{}_HASH={}", label, short8);

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = Path::new(&out_dir).join(format!("{}_hash.rs", label.to_lowercase()));
    fs::write(
        &dest,
        format!(
            "/// {} bundle content hash, computed at build time from assets/{}.\n\
             /// Use as `?v={{hash}}` query string to bust browser caches.\n\
             pub const APP_{}_HASH: &str = \"{hash}\";\n",
            label,
            file_name,
            label,
            hash = short8,
        ),
    )
    .unwrap_or_else(|e| panic!("failed to write {}_hash.rs: {}", label.to_lowercase(), e));
}