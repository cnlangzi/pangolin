#![allow(unused_imports)]
//! Pangolin tunnel node (tun) library.
//!
//! Re-exports the public API: `TunnelClient`, `Config`, `frame`,
//! `test_ws_server`, plus the `config` module (loaded from `tun.yml`).

pub mod client;
pub mod config;
pub mod frame;
pub mod test_ws_server;

pub use client::{validate_config, Config, TunnelClient};
#[allow(unused_imports)]
pub use frame::{
    deserialize_msgpack, serialize_msgpack, TunnelFrame, TunnelRequestFrame, TunnelResponseFrame,
};
pub use test_ws_server::TestWsServer;
