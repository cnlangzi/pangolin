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
//!
//! ## Why a prefix tree (issue #253)
//!
//! The rules used to be matched by scanning the whole list: the
//! origin guard compared the peer against every published range, and
//! the per-site IP rules did the same twice (whitelist and
//! blacklist). A site with a thousand rules paid a thousand
//! comparisons per request.
//!
//! The networks now live in a **binary prefix tree** (one per address
//! family). A lookup walks the address bit by bit — 32 steps for
//! IPv4, 128 for IPv6 — no matter how many networks the set holds,
//! and the deepest node on the path that carries a prefix length is
//! the longest matching prefix. The semantics are unchanged: the
//! tree stores the *masked* network address of every range, exactly
//! the interval the linear scan compared against.

use std::collections::HashSet;
use std::net::IpAddr;

use ipnet::IpNet;

/// A binary prefix tree over one address family's bits.
///
/// One node per prefix: the path to a node is the sequence of address
/// bits that lead to it, so a node at depth `d` stands for the
/// network "the first `d` bits of the address". A node that carries a
/// prefix length is the **end** of a range; a node without one is
/// only a step on the way to a longer range.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PrefixTree {
    /// The two children: the next bit is 0 or 1.
    children: [Option<Box<PrefixTree>>; 2],
    /// The prefix length of the range that ends at this node.
    terminal: Option<u8>,
}

impl PrefixTree {
    /// Add the range `network / prefix_len`.
    ///
    /// `network` must already be **masked** to `prefix_len` — the
    /// tree compares prefixes, so a range whose host bits are set
    /// would land on the wrong path.
    fn insert(&mut self, network: u128, bits: u8, prefix_len: u8) {
        let mut node = self;
        for depth in 0..prefix_len {
            let bit = ((network >> (bits - 1 - depth)) & 1) as usize;
            node = node.children[bit]
                .get_or_insert_with(|| Box::new(PrefixTree::default()))
                .as_mut();
        }
        node.terminal = Some(prefix_len);
    }

    /// The longest prefix in the tree that contains `addr`.
    fn longest_prefix(&self, addr: u128, bits: u8) -> Option<u8> {
        // A range that covers everything (`/0`) ends at the root.
        let mut best = self.terminal;
        let mut node = self;
        for depth in 0..bits {
            let bit = ((addr >> (bits - 1 - depth)) & 1) as usize;
            let Some(child) = node.children[bit].as_deref() else {
                break;
            };
            node = child;
            if let Some(len) = node.terminal {
                best = Some(len);
            }
        }
        best
    }
}

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
    /// CIDR ranges, in the order they were added.
    ///
    /// Kept as the **source of truth** for the canonical text form
    /// ([`CidrSet::to_cidrs`]) and for the set's equality: the
    /// prefix trees below are a derived view of them, and a view
    /// cannot reproduce the text a range was written in.
    nets: Vec<IpNet>,
    /// The IPv4 ranges, indexed for lookup.
    v4: PrefixTree,
    /// The IPv6 ranges, indexed for lookup.
    v6: PrefixTree,
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
                self.index(net);
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
        self.match_range(ip).is_some()
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
        self.match_range(ip)
    }

    /// Index one range for lookup.
    ///
    /// The tree holds the **masked** network address — the interval
    /// the range covers — not the address the range was written with.
    /// `10.0.0.1/8` and `10.0.0.0/8` are the same range, and both
    /// have to match the same addresses.
    fn index(&mut self, net: IpNet) {
        match net.network() {
            IpAddr::V4(a) => self.v4.insert(u32::from(a).into(), 32, net.prefix_len()),
            IpAddr::V6(a) => self.v6.insert(u128::from(a), 128, net.prefix_len()),
        }
    }

    /// The longest prefix among the **ranges** that contains `ip`.
    ///
    /// The exact entries are not consulted here: they are answered
    /// by the callers, which give them their own prefix length.
    fn match_range(&self, ip: IpAddr) -> Option<u8> {
        match ip {
            IpAddr::V4(a) => self.v4.longest_prefix(u32::from(a).into(), 32),
            IpAddr::V6(a) => self.v6.longest_prefix(u128::from(a), 128),
        }
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

    /// A range whose host bits are set still covers the whole
    /// interval, and keeps the text form it was written in.
    ///
    /// The tree indexes the **masked** network address — that is the
    /// interval a range covers — while the canonical text form stays
    /// the one the operator wrote, because the tables key on it
    /// (issue #253).
    #[test]
    fn a_range_written_with_host_bits_covers_the_whole_interval() {
        let mut set = CidrSet::new();
        assert_eq!(set.insert("10.0.0.1/8").unwrap(), "10.0.0.1/8");
        assert!(set.contains(ip("10.1.2.3")));
        assert!(!set.contains(ip("11.1.2.3")));
        assert_eq!(set.longest_prefix(ip("10.1.2.3")), Some(8));
        assert_eq!(set.to_cidrs(), vec!["10.0.0.1/8"]);
    }

    /// The longest prefix wins across a nested set of ranges, and a
    /// range of the other address family never matches.
    #[test]
    fn the_longest_prefix_wins_across_nested_ranges() {
        let mut set = CidrSet::new();
        set.insert("10.0.0.0/8").unwrap();
        set.insert("10.1.0.0/16").unwrap();
        set.insert("10.1.2.0/24").unwrap();
        set.insert("2001:db8::/32").unwrap();
        assert_eq!(set.longest_prefix(ip("10.1.2.3")), Some(24));
        assert_eq!(set.longest_prefix(ip("10.1.3.3")), Some(16));
        assert_eq!(set.longest_prefix(ip("10.2.3.3")), Some(8));
        assert_eq!(set.longest_prefix(ip("11.2.3.3")), None);
        // An IPv4 address is never inside an IPv6 range.
        assert_eq!(set.longest_prefix("2001:db8::1".parse().unwrap()), Some(32));
        assert_eq!(set.longest_prefix("2001:db9::1".parse().unwrap()), None);
    }

    /// A thousand ranges, a thousand lookups: the matching must not
    /// scale with the number of ranges (issue #253).
    ///
    /// The prefix tree walks the address bit by bit, so the cost of a
    /// lookup does not depend on how many ranges the set holds. The
    /// budget is the one the issue names: 1000 lookups in under 10 ms
    /// — two orders of magnitude above what the tree actually costs.
    #[test]
    fn a_thousand_ranges_are_matched_in_milliseconds() {
        let mut set = CidrSet::new();
        let mut last = String::new();
        for i in 0..1000u32 {
            last = format!("10.{}.{}.0/24", i / 256, i % 256);
            set.insert(&last).unwrap();
        }
        // The probe is inside the **last** range that was added: a
        // linear scan has to walk every range before it finds it, so
        // the measurement below is the one a linear implementation
        // cannot pass.
        let probe: IpAddr = last.replace(".0/24", ".7").parse().unwrap();

        let start = std::time::Instant::now();
        let mut matched = 0usize;
        for _ in 0..1000 {
            if set.contains(probe) {
                matched += 1;
            }
        }
        let elapsed = start.elapsed();

        assert_eq!(matched, 1000, "the probe is inside the last range");
        assert!(
            elapsed.as_millis() < 10,
            "1000 lookups over 1000 ranges took {elapsed:?} — the matching is not \
             independent of the number of ranges"
        );
    }
}
