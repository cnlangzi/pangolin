use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;
use std::path::Path;

/// Computes a short hash of the CSS file at compile time.
/// Outputs:
///   - cargo:rustc-env=APP_CSS_HASH=<8 chars>
///   - $OUT_DIR/css_hash.rs containing `pub const APP_CSS_HASH: &str = ...`
fn main() {
    // Locate the CSS file. In a workspace, the cargo CWD is the crate's directory.
    // We try several relative paths to find the workspace-root assets/app.css.
    let candidates = [
        "assets/app.css",
        "../assets/app.css",
        "../../assets/app.css",
        "../../../assets/app.css",
    ];

    let css_path = candidates
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .expect("could not locate assets/app.css — checked several relative paths");

    println!("cargo:rerun-if-changed={}", css_path.display());
    // Also watch the source tailwind file so rebuilding CSS triggers a rebuild.
    if Path::new("assets/tailwindcss.css").exists() {
        println!("cargo:rerun-if-changed=assets/tailwindcss.css");
    } else if Path::new("../assets/tailwindcss.css").exists() {
        println!("cargo:rerun-if-changed=../assets/tailwindcss.css");
    }

    let css = fs::read(css_path).expect("failed to read app.css");
    let mut hasher = DefaultHasher::new();
    css.hash(&mut hasher);
    let full = format!("{:x}", hasher.finish());
    let short8 = full[..8].to_string();

    // Emit env var for use in `env!("APP_CSS_HASH")` at compile time.
    println!("cargo:rustc-env=APP_CSS_HASH={}", short8);

    // Also write a generated const file for the admin crate to include.
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = Path::new(&out_dir).join("css_hash.rs");
    fs::write(
        &dest,
        format!(
            "/// CSS content hash, computed at build time from assets/app.css.\n\
             /// Use as `?v={hash}` query string to bust browser caches.\n\
             pub const APP_CSS_HASH: &str = \"{hash}\";\n",
            hash = short8
        ),
    )
    .expect("failed to write css_hash.rs");
}