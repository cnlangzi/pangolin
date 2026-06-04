//! Admin UI — shared App state types.
//!
//! Re-exports `App` from `pangolin_core`. Both ngx (the real owner) and
//! admin (a consumer via Arc<App>) use the identical type.

pub use pangolin_core::App;