#![allow(unused_imports)]
//! Pangolin tunnel node (tun) library.
//!
//! Re-exports the public API: `TunnelClient`, `TunConfig`, plus
//! the `config` module (loaded from `tun.yml`).

pub mod client;
pub mod config;

pub use client::TunnelClient;
pub use config::TunConfig;
