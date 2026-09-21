//! Verify search-engine / AI / social bots via User-Agent + IP ownership.
//!
//! Hot path ([`Validator::verify`]) is synchronous and allocation-light:
//! case-sensitive word-boundary UA match, then CIDR / RDNS-cache lookup.
//! Cold reverse-DNS only warms the persistent cache — the request that
//! triggered the lookup is treated as **not verified** (fail closed).
//!
//! See the upstream Go library: <https://github.com/cnlangzi/knownbots>.

mod bot;
mod lru;
mod parser;
mod rdns;
mod ua;
mod validator;

pub use bot::{Bot, BotKind, load_bots, vendor_for};
pub use rdns::match_domain;
pub use ua::{contains_word, find_bot_by_ua};
pub use validator::{Validator, ValidatorOptions, VerifyResult, VerifyStatus};
