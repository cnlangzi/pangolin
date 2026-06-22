//! Admin route handlers — one module per resource.
//!
//! ## Layout (xun-style)
//!
//! Each resource lives in its own subdirectory with the following files:
//! - `mod.rs`     — sub-module re-exports and resource-level helpers
//! - `pages.rs`   — GET full pages
//! - `views.rs`   — GET HTMX partials (returns HTML fragments)
//! - `mutate.rs`  — POST / PUT / DELETE handlers
//!
//! The flat `auth` and `dashboard` modules are single-resource pages that
//! have no children (no HTMX partials, no POST), so they stay at the top
//! level.
//!
//! The `helpers` module exposes `ok_html`, `redirect`, `require_param`,
//! and `flash_error` — small cross-resource utilities that were
//! duplicated across every resource before the xun split.

pub mod auth;
pub mod certs;
pub mod dashboard;
pub mod dns;
pub mod domains;
pub mod helpers;
pub mod logs;
pub mod sites;
pub mod system;
pub mod tun;
