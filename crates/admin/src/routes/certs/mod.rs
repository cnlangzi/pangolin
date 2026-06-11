//! Certs route — list / new / delete.
//!
//! Split per xun-style resource layout:
//! - `pages.rs`  — GET full pages (`/certs`, `/certs/new`)
//! - `views.rs`  — GET HTMX partials (none for certs at the moment)
//! - `mutate.rs` — POST / DELETE handlers

pub mod helpers;
pub mod mutate;
pub mod pages;
pub mod views;

pub use mutate::{handle_create, handle_delete};
pub use pages::{render, render_create_page};
