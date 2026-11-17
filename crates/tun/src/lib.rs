//! Pangolin tunnel node (tun) library.
//!
//! Re-exports the public API: `TunConfig`, the `config` module
//! (loaded from `tun.yml`), and the transport selector the
//! configuration uses.
//!
//! The tunnel's wire protocol and the client itself live in the
//! published `pangolin-tun` crate (issue #159); this crate is the
//! configuration and the binary that ships it.

pub mod config;

pub use config::TunConfig;
pub use pangolin_tun::TransportSelector;
