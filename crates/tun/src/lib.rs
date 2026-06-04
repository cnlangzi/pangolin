//! Pangolin tunnel node (tun) library.
//!
//! Re-exports the public API: `TunnelClient`, `Config`, `frame`, `test_ws_server`.

pub mod client;
pub mod frame;
pub mod test_ws_server;

pub use client::{TunnelClient, Config, validate_config};
pub use frame::{
    deserialize_msgpack, serialize_msgpack,
    TunnelFrame, TunnelRequestFrame, TunnelResponseFrame,
};
pub use test_ws_server::TestWsServer;