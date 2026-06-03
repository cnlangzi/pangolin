//! Tunnel protocol frames — shared with ngx via pangolin-core.
//!
//! We re-export the types from pangolin-core so that tun stays in sync
//! with ngx's frame definitions. This avoids duplication and ensures
//! both sides use identical structs.

pub use pangolin_core::compress::{deflate_decode, deflate_encode};
#[allow(unused)]
pub use pangolin_core::{
    deserialize_msgpack, serialize_frames, serialize_msgpack, TunnelFrame, TunnelRequestFrame,
    TunnelResponseFrame,
};
