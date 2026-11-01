//! A set of IP addresses and CIDR ranges.
//!
//! One implementation, two users:
//!
//! * the per-site IP rules (`ratelimit::IpRules`) — a whitelist and a
//!   blacklist per site, matched by **longest prefix**;
//! * the origin guard's allowlist (`provider::SiteProviderConfig`) —
//!   the provider's published ranges ∪ the site's manual ranges,
//!   matched by plain membership.
//!
//! Both need the same parse semantics — a bare address is an exact
//! match, a CIDR is a range — so the parsing and the matching live
//! here rather than being written twice.
//!
//! The set is deliberately not concurrent: it is built once (at
//! index-build time or at range-fetch time) and then handed to the
//! request path as an `Arc`, so a reader never takes a lock.

use std::collections::HashSet;
use std::net::IpAddr;

use ipnet::IpNet;

/// A set of IP addresses and CIDR ranges.
///
/// Membership is by longest prefix: a `/32` beats a `/8`. That is what
/// the IP rules need (a whitelist `/8` must not shadow a blacklist
/// `/32`); for plain membership — the origin guard — the rule reduces
/// to "is the address covered by any entry".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CidrSet {
    /// Bare addresses (an exact match).
    exact: HashSet<IpAddr>,
    /// CIDR ranges.
    nets: Vec<IpNet>,
}

impl CidrSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an address or CIDR range.
    ///
    /// `"1.2.3.4"` is stored as an exact address, `"10.0.0.0/8"` as a
    /// range. Returns the normalised text form on success, or a
    /// human-readable error naming the offending value for a malformed
    /// one.
    pub fn insert(&mut self, cidr: &str) -> Result<String, String> {
        let trimmed = cidr.trim();
        if trimmed.is_empty() {
            return Err("empty CIDR".to_string());
        }
        if trimmed.contains('/') {
            let net: IpNet = trimmed
                .parse()
                .map_err(|e| format!("invalid CIDR '{trimmed}': {e}"))?;
            let text = net.to_string();
            if !self.nets.contains(&net) {
                self.nets.push(net);
            }
            Ok(text)
        } else {
            let ip: IpAddr = trimmed
                .parse()
                .map_err(|e| format!("invalid IP address '{trimmed}': {e}"))?;
            self.exact.insert(ip);
            Ok(ip.to_string())
        }
    }

    /// True when the set holds no entries at all.
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.nets.is_empty()
    }

    /// Number of entries in the set.
    pub fn len(&self) -> usize {
        self.exact.len() + self.nets.len()
    }

    /// True when `ip` is covered by the set.
    ///
    /// An exact address matches itself; a CIDR matches every address
    /// inside it. The two families never overlap in practice (an
    /// address inside a range is not also stored as an exact entry),
    /// but both are checked so the answer is right either way.
    pub fn contains(&self, ip: IpAddr) -> bool {
        if self.exact.contains(&ip) {
            return true;
        }
        self.nets.iter().any(|n| n.contains(&ip))
    }

    /// The length of the longest prefix in the set that contains
    /// `ip`, or `None` when the set does not match.
    ///
    /// An exact address matches with a prefix length of 32 (IPv4) or
    /// 128 (IPv6).
    pub fn longest_prefix(&self, ip: IpAddr) -> Option<u8> {
        if self.exact.contains(&ip) {
            return Some(if ip.is_ipv4() { 32 } else { 128 });
        }
        self.nets
            .iter()
            .filter(|n| n.contains(&ip))
            .map(|n| n.prefix_len())
            .max()
    }

    /// The canonical text form of every entry — the form the
    /// `site_origin_allow` / `provider_ip_lists` tables store.
    ///
    /// Sorted so the persisted form is stable between runs (the
    /// tables key on the text, so an unstable order would create
    /// spurious row churn).
    pub fn to_cidrs(&self) -> Vec<String> {
        let mut out: Vec<String> = self.exact.iter().map(|ip| ip.to_string()).collect();
        out.extend(self.nets.iter().map(|n| n.to_string()));
        out.sort();
        out
    }
}

/// Normalise the text form of an address or range (`1.2.3.4` or
/// `10.0.0.0/8`) into its canonical form.
///
/// This is the one place the canonicalisation lives: the admin UI
/// stores what it returns, and the loaders parse it back.
pub fn normalize_cidr(cidr: &str) -> Result<String, String> {
    let mut set = CidrSet::new();
    set.insert(cidr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn an_address_is_stored_as_an_exact_entry() {
        let mut set = CidrSet::new();
        assert_eq!(set.insert("1.2.3.4").unwrap(), "1.2.3.4");
        assert_eq!(set.len(), 1);
        assert!(set.contains(ip("1.2.3.4")));
        assert!(!set.contains(ip("1.2.3.5")));
    }

    #[test]
    fn a_range_is_normalised() {
        let mut set = CidrSet::new();
        assert_eq!(set.insert("10.0.0.0/8").unwrap(), "10.0.0.0/8");
        assert!(set.contains(ip("10.1.2.3")));
        assert!(!set.contains(ip("11.1.2.3")));
    }

    #[test]
    fn an_ipv6_range_is_supported() {
        let mut set = CidrSet::new();
        assert_eq!(set.insert("2001:db8::/32").unwrap(), "2001:db8::/32");
        assert!(set.contains(ip("2001:db8::1")));
        assert!(!set.contains(ip("2001:db9::1")));
    }

    #[test]
    fn a_malformed_value_is_rejected_and_names_the_offender() {
        let mut set = CidrSet::new();
        let err = set.insert("not-an-ip").unwrap_err();
        assert!(err.contains("not-an-ip"), "{err}");
        assert!(set.is_empty());

        let err = set.insert("10.0.0.0/99").unwrap_err();
        assert!(err.contains("10.0.0.0/99"), "{err}");
        assert!(set.is_empty());
    }

    #[test]
    fn an_empty_value_is_rejected() {
        let mut set = CidrSet::new();
        assert!(set.insert("   ").is_err());
        assert!(set.is_empty());
    }

    #[test]
    fn the_same_entry_is_not_stored_twice() {
        let mut set = CidrSet::new();
        set.insert("10.0.0.0/8").unwrap();
        set.insert("10.0.0.0/8").unwrap();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn longest_prefix_beats_a_shorter_one() {
        let mut set = CidrSet::new();
        set.insert("10.0.0.0/8").unwrap();
        set.insert("10.1.0.0/16").unwrap();
        assert_eq!(set.longest_prefix(ip("10.1.2.3")), Some(16));
        assert_eq!(set.longest_prefix(ip("10.2.2.3")), Some(8));
        assert_eq!(set.longest_prefix(ip("11.2.2.3")), None);
    }

    #[test]
    fn an_exact_address_matches_with_a_full_prefix() {
        let mut set = CidrSet::new();
        set.insert("1.2.3.4").unwrap();
        assert_eq!(set.longest_prefix(ip("1.2.3.4")), Some(32));
    }

    #[test]
    fn the_canonical_form_is_stable_and_sorted() {
        let mut set = CidrSet::new();
        set.insert("10.0.0.0/8").unwrap();
        set.insert("1.2.3.4").unwrap();
        set.insert("2001:db8::/32").unwrap();
        assert_eq!(
            set.to_cidrs(),
            vec!["1.2.3.4", "10.0.0.0/8", "2001:db8::/32"]
        );
    }

    #[test]
    fn an_empty_set_contains_nothing() {
        let set = CidrSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert!(!set.contains(ip("1.2.3.4")));
        assert!(set.to_cidrs().is_empty());
    }
}
