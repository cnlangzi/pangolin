//! DNS provider route — list / new / edit / delete / test.
//!
//! Split per xun-style resource layout:
//! - `pages.rs`  — GET full pages (`/dns`, `/dns/new`, `/dns/{name}/edit`)
//! - `views.rs`  — GET HTMX partials (none for dns at the moment)
//! - `mutate.rs` — POST / DELETE / test handlers

pub mod helpers;
pub mod mutate;
pub mod pages;
pub mod views;

pub use mutate::{
    api_handle_delete, handle_create, handle_delete, handle_test, handle_update,
};
pub use pages::{render, render_create_page, render_edit_page};
