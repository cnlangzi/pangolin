//! Pangolin tunnel node (tun) library.
//!
//! Re-exports the public API: `TunnelClient`, `Config`, `frame`, `mock_ngx`.

pub mod client;
pub mod frame;
pub mod mock_ngx;

pub use client::{TunnelClient, Config, validate_config};
pub use frame::{
    deserialize_msgpack, serialize_msgpack,
    TunnelFrame, TunnelRequestFrame, TunnelResponseFrame,
};