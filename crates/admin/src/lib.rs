// Pangolin admin UI — SSR templates (askama) + htmx.
// No JS framework. Tailwindcss compiled by npm run build.

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
