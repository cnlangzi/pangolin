//! The backend address resolver.
//!
//! A backend is addressed either by an **IP literal**
//! (`http://10.0.0.1:8080`) or by a **DNS name**
//! (`http://api.internal:8080`). pingora's `HttpPeer::new` resolves
//! its argument eagerly, with `std::net::ToSocketAddrs` — for a name
//! that is a synchronous, blocking `getaddrinfo`.
//!
//! `upstream_peer` is a per-request hook, so handing it a name meant
//! **one DNS query per request**, on a worker thread, and a panic
//! (not a 502) when the name did not resolve (issue #253).
//!
//! This module resolves a name **once** and caches the addresses:
//!
//! * the request path never resolves a name — it reads the cache;
//! * a name that resolves to several addresses is walked on retry, so
//!   one dead address does not take the backend down;
//! * a name that does not resolve is an **error**, not a panic;
//! * an IP literal is parsed, never resolved, and never cached.
//!
//! The resolver itself is [hickory-resolver](https://docs.rs/hickory-resolver)
//! 0.26 — asynchronous, so a resolution never blocks a worker thread —
//! configured from the system's `/etc/resolv.conf`, because a backend
//! is often an **internal** name the public resolvers cannot answer.
//!
//! The resolver reads `/etc/hosts` **once**, when it is built, and
//! consults it for every lookup. A name added to the hosts file while
//! the gateway runs is therefore not resolved until the gateway
//! restarts — the old `getaddrinfo` path re-read the file per lookup.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::system_conf::read_system_conf;

/// How long a cached address list is served before it is resolved
/// again.
///
/// The DNS record's own TTL wins when the resolver reports one; this
/// is the floor, so a record with a very short TTL does not turn the
/// request path back into a resolver.
const MIN_TTL: Duration = Duration::from_secs(5);

/// How many background refresh failures pass between two warnings.
///
/// The first failure is logged, then every n-th — a broken
/// nameserver must not flood the log.
const REFRESH_LOG_EVERY: u64 = 100;

/// The key a backend's addresses are cached under.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackendKey {
    /// The host name, lower-cased — DNS names are case-insensitive.
    pub host: String,
    /// The port the backend listens on.
    pub port: u16,
}

/// The addresses one backend host resolved to.
#[derive(Debug, Clone)]
pub struct ResolvedBackend {
    /// Every address the name resolved to, in the order the resolver
    /// returned them.
    ///
    /// The first one is the address the first attempt dials; a retry
    /// walks the list (see [`BackendResolver::address`]).
    pub addrs: Vec<SocketAddr>,
    /// When the entry stops being valid.
    pub valid_until: Instant,
}

/// Why a backend's address could not be produced.
///
/// Cloneable so that the **result** of one resolution attempt can be
/// handed to everyone who waited for it — a failure included.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ResolveError {
    /// The name has no address record.
    #[error("backend host {host} has no address record")]
    NoAddress {
        /// The host that did not resolve.
        host: String,
    },
    /// The resolver failed — a timeout, a broken nameserver, a
    /// malformed answer.
    #[error("backend host {host} could not be resolved: {reason}")]
    Failed {
        /// The host that did not resolve.
        host: String,
        /// What the resolver said.
        reason: String,
    },
    /// The resolver itself could not be built.
    #[error("the backend resolver could not be built: {reason}")]
    Unavailable {
        /// What went wrong.
        reason: String,
    },
}

/// The name resolution the resolver performs.
///
/// A trait so the cache can be tested with a resolver that counts its
/// calls — the property the request path depends on is "a name is
/// resolved once, not once per request".
#[async_trait]
pub trait BackendLookup: Send + Sync {
    /// The addresses `host` resolves to, and how long they are valid.
    async fn lookup(&self, host: &str) -> Result<(Vec<IpAddr>, Duration), String>;
}

/// A [`BackendLookup`] backed by hickory-resolver.
pub struct HickoryLookup {
    resolver: TokioResolver,
}

impl HickoryLookup {
    /// Build a lookup from an explicit resolver configuration.
    pub fn new(config: ResolverConfig, opts: ResolverOpts) -> Result<Self, ResolveError> {
        let resolver = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
            .with_options(opts)
            .build()
            .map_err(|e| ResolveError::Unavailable {
                reason: e.to_string(),
            })?;
        Ok(Self { resolver })
    }

    /// Build a lookup from the system's `/etc/resolv.conf`.
    ///
    /// The system configuration is the right one for a backend: the
    /// name is often internal, and only the operator's own
    /// nameservers can answer it. `ResolverConfig::default()` would
    /// come with **no** nameservers at all, so it is not a fallback.
    pub fn from_system_conf() -> Result<Self, ResolveError> {
        let (config, opts) = read_system_conf().map_err(|e| ResolveError::Unavailable {
            reason: e.to_string(),
        })?;
        Self::new(config, opts)
    }
}

#[async_trait]
impl BackendLookup for HickoryLookup {
    async fn lookup(&self, host: &str) -> Result<(Vec<IpAddr>, Duration), String> {
        let lookup = self
            .resolver
            .lookup_ip(host)
            .await
            .map_err(|e| e.to_string())?;
        let addrs: Vec<IpAddr> = lookup.iter().collect();
        if addrs.is_empty() {
            return Err("no address record".to_string());
        }
        // The record's own TTL bounds how long the answer may be
        // served; `MIN_TTL` is the floor.
        let ttl = lookup
            .valid_until()
            .saturating_duration_since(Instant::now())
            .max(MIN_TTL);
        Ok((addrs, ttl))
    }
}

/// A lookup that cannot resolve anything.
///
/// Used when the system's resolver configuration could not be read:
/// the gateway still starts and still serves backends addressed by an
/// IP literal, and a backend addressed by a name answers 502 with the
/// reason.
pub struct UnavailableLookup {
    reason: String,
}

impl UnavailableLookup {
    /// The lookup that fails with `reason`.
    pub fn new(reason: String) -> Self {
        Self { reason }
    }
}

#[async_trait]
impl BackendLookup for UnavailableLookup {
    async fn lookup(&self, _host: &str) -> Result<(Vec<IpAddr>, Duration), String> {
        Err(self.reason.clone())
    }
}

/// The result of one resolution attempt.
///
/// Shared by everyone who waited for that attempt: a **failure** is
/// shared too, so a backend that does not resolve does not make every
/// waiter pay the resolver's timeout in turn (issue #253).
type AttemptResult = Result<Arc<ResolvedBackend>, ResolveError>;

/// Resolves backend host names to the socket addresses the gateway
/// dials.
///
/// The cache is the whole point: the request path reads it and never
/// resolves a name. A miss resolves **once** — concurrent requests
/// for the same host wait for that one resolution instead of each
/// starting their own — and an expired entry is served as it is while
/// it is refreshed in the background.
pub struct BackendResolver {
    lookup: Arc<dyn BackendLookup>,
    /// The cached addresses, per backend.
    cache: std::sync::RwLock<HashMap<BackendKey, Arc<ResolvedBackend>>>,
    /// The resolution attempt in flight per host — the single-flight
    /// seam.
    ///
    /// The **sender** is held here so that a request that arrives
    /// while an attempt runs joins that attempt instead of starting
    /// its own: one resolution, one answer, one failure — shared by
    /// everyone who waited for it.
    ///
    /// One entry per host that is being resolved; the map is bounded
    /// by the number of distinct backends, which is bounded by the
    /// number of sites.
    inflight:
        std::sync::Mutex<HashMap<BackendKey, tokio::sync::watch::Sender<Option<AttemptResult>>>>,
    /// How many background refreshes failed — the counter that keeps
    /// the warning at a bounded rate.
    refresh_failures: std::sync::Mutex<u64>,
}

impl BackendResolver {
    /// Build a resolver over a name lookup.
    pub fn new(lookup: Arc<dyn BackendLookup>) -> Self {
        Self {
            lookup,
            cache: std::sync::RwLock::new(HashMap::new()),
            inflight: std::sync::Mutex::new(HashMap::new()),
            refresh_failures: std::sync::Mutex::new(0),
        }
    }

    /// Build a resolver over the system's `/etc/resolv.conf`.
    pub fn from_system_conf() -> Result<Self, ResolveError> {
        Ok(Self::new(Arc::new(HickoryLookup::from_system_conf()?)))
    }

    /// The socket address to dial for one attempt at `host`.
    ///
    /// An IP literal is parsed and returned as is — it is never
    /// resolved and never cached. A name is served from the cache; a
    /// miss resolves it once and caches the answer, and an expired
    /// entry is served as it is while it is refreshed in the
    /// background.
    ///
    /// `attempt` walks the addresses of a name that resolved to
    /// several: the first attempt dials the first address, a retry
    /// dials the next one. That is what makes a name with a dead
    /// address survive.
    pub async fn address(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        attempt: usize,
    ) -> Result<SocketAddr, ResolveError> {
        // An IP literal is not a name: no lookup, no cache.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, port));
        }
        let key = BackendKey {
            host: host.to_ascii_lowercase(),
            port,
        };
        let resolved = self.resolve(&key).await?;
        let index = attempt % resolved.addrs.len();
        Ok(resolved.addrs[index])
    }

    /// The cached addresses of a backend, resolving them if the cache
    /// has none.
    ///
    /// A **fresh** entry is served straight away. A **stale** entry —
    /// one whose TTL has passed — is served as it is and refreshed in
    /// the background: the request does not wait for the resolver
    /// (stale-while-revalidate). Only a backend the gateway has never
    /// resolved makes a request wait for the resolver.
    async fn resolve(
        self: &Arc<Self>,
        key: &BackendKey,
    ) -> Result<Arc<ResolvedBackend>, ResolveError> {
        if let Some(fresh) = self.fresh(key) {
            return Ok(fresh);
        }
        if let Some(stale) = self.stale(key) {
            self.refresh_in_background(key);
            return Ok(stale);
        }
        // Single-flight: the first request for a host resolves it and
        // the others **join that attempt** — one resolution, one
        // answer. A failure is shared too: a backend that does not
        // resolve must not make every waiter pay the resolver's
        // timeout in turn.
        let (leader, sender, mut receiver) = self.join_attempt(key);
        if !leader {
            // Wait for the attempt's result.
            let _ = receiver.wait_for(|result| result.is_some()).await;
            return match receiver.borrow().clone() {
                Some(result) => result,
                // The attempt died without publishing a result — a
                // panic in the lookup. The waiter answers with an
                // error rather than panicking itself.
                None => Err(ResolveError::Failed {
                    host: key.host.clone(),
                    reason: "the resolution attempt did not complete".to_string(),
                }),
            };
        }
        // The leader resolves, publishes the answer to the waiters
        // and retires the attempt: the next request starts a fresh
        // one, so a name that starts resolving is picked up without a
        // restart.
        let result = self.resolve_now(key).await;
        let _ = sender.send(Some(result.clone()));
        self.retire_attempt(key);
        if let Ok(resolved) = &result {
            self.store(key, resolved.clone());
        }
        result
    }

    /// Join the resolution attempt in flight for a backend, or start
    /// one.
    ///
    /// Returns `(is_leader, sender, receiver)`: the leader resolves
    /// and publishes, the others wait for what it publishes.
    fn join_attempt(
        &self,
        key: &BackendKey,
    ) -> (
        bool,
        tokio::sync::watch::Sender<Option<AttemptResult>>,
        tokio::sync::watch::Receiver<Option<AttemptResult>>,
    ) {
        let mut inflight = match self.inflight.lock() {
            Ok(guard) => guard,
            // A poisoned lock must not take the gateway down: the map
            // is a registry of attempts, and a fresh one does the job.
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(sender) = inflight.get(key) {
            return (false, sender.clone(), sender.subscribe());
        }
        let (sender, receiver) = tokio::sync::watch::channel(None);
        inflight.insert(key.clone(), sender.clone());
        (true, sender, receiver)
    }

    /// Retire the attempt in flight for a backend.
    ///
    /// The attempt is retired once it published its result: the next
    /// request starts a fresh one.
    fn retire_attempt(&self, key: &BackendKey) {
        let mut inflight = match self.inflight.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inflight.remove(key);
    }

    /// Resolve a backend now.
    async fn resolve_now(&self, key: &BackendKey) -> Result<Arc<ResolvedBackend>, ResolveError> {
        let (addrs, ttl) =
            self.lookup
                .lookup(&key.host)
                .await
                .map_err(|reason| ResolveError::Failed {
                    host: key.host.clone(),
                    reason,
                })?;
        if addrs.is_empty() {
            return Err(ResolveError::NoAddress {
                host: key.host.clone(),
            });
        }
        Ok(Arc::new(ResolvedBackend {
            addrs: addrs
                .into_iter()
                .map(|ip| SocketAddr::new(ip, key.port))
                .collect(),
            valid_until: Instant::now() + ttl,
        }))
    }

    /// Refresh a stale entry in the background.
    ///
    /// The stale addresses stay in force: a refresh that fails keeps
    /// the old entry, and the failure is logged at a bounded rate —
    /// one line for the first failure and then every
    /// [`REFRESH_LOG_EVERY`]th, so a broken nameserver cannot flood
    /// the log.
    fn refresh_in_background(self: &Arc<Self>, key: &BackendKey) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime to run the refresh on — the stale entry stays
            // and the next request refreshes it in the foreground.
            return;
        };
        let this = self.clone();
        let key = key.clone();
        handle.spawn(async move {
            // Join the attempt in flight: a refresh that is already
            // running answers this one.
            let (leader, sender, mut receiver) = this.join_attempt(&key);
            if !leader {
                let _ = receiver.wait_for(|result| result.is_some()).await;
                return;
            }
            let result = this.resolve_now(&key).await;
            let _ = sender.send(Some(result.clone()));
            this.retire_attempt(&key);
            match result {
                Ok(resolved) => this.store(&key, resolved),
                Err(e) => {
                    // The stale addresses stay in force; the failure
                    // is logged at a bounded rate.
                    if this.record_refresh_failure() {
                        log::warn!("backend resolver: {} could not be refreshed: {e}", key.host);
                    }
                }
            }
        });
    }

    /// Count one refresh failure; `true` when it should be logged.
    fn record_refresh_failure(&self) -> bool {
        let mut failures = match self.refresh_failures.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *failures += 1;
        let n = *failures;
        n == 1 || n.is_multiple_of(REFRESH_LOG_EVERY)
    }

    /// The cached addresses of a backend, when they are still valid.
    fn fresh(&self, key: &BackendKey) -> Option<Arc<ResolvedBackend>> {
        let cache = self.cache.read().ok()?;
        let entry = cache.get(key)?;
        if entry.valid_until > Instant::now() {
            return Some(entry.clone());
        }
        None
    }

    /// The cached addresses of a backend, whether they are still valid
    /// or not.
    fn stale(&self, key: &BackendKey) -> Option<Arc<ResolvedBackend>> {
        let cache = self.cache.read().ok()?;
        cache.get(key).cloned()
    }

    /// Publish the addresses of a backend.
    fn store(&self, key: &BackendKey, resolved: Arc<ResolvedBackend>) {
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(key.clone(), resolved);
        }
    }

    /// Expire a cached entry — the test helper for the
    /// stale-while-revalidate path.
    #[cfg(test)]
    fn expire(&self, key: &BackendKey) {
        if let Ok(mut cache) = self.cache.write()
            && let Some(entry) = cache.get(key)
        {
            let mut stale = (**entry).clone();
            stale.valid_until = Instant::now() - Duration::from_secs(1);
            cache.insert(key.clone(), Arc::new(stale));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A lookup that counts its calls and answers from a fixed table.
    struct CountingLookup {
        calls: AtomicUsize,
        addrs: Vec<IpAddr>,
        ttl: Duration,
    }

    impl CountingLookup {
        fn new(addrs: Vec<IpAddr>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                addrs,
                ttl: Duration::from_secs(60),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl BackendLookup for CountingLookup {
        async fn lookup(&self, _host: &str) -> Result<(Vec<IpAddr>, Duration), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok((self.addrs.clone(), self.ttl))
        }
    }

    /// A name is resolved **once**, no matter how many requests ask
    /// for it (issue #253).
    ///
    /// The request path used to hand the name to `HttpPeer::new`,
    /// which resolved it — synchronously, on the worker thread — on
    /// every single request.
    #[tokio::test]
    async fn a_name_is_resolved_once_and_then_served_from_the_cache() {
        let lookup = CountingLookup::new(vec!["203.0.113.7".parse().unwrap()]);
        let resolver = Arc::new(BackendResolver::new(lookup.clone()));

        for _ in 0..10 {
            let addr = resolver.address("api.internal", 8080, 0).await.unwrap();
            assert_eq!(addr, "203.0.113.7:8080".parse::<SocketAddr>().unwrap());
        }

        assert_eq!(
            lookup.calls(),
            1,
            "ten requests must not mean ten DNS queries"
        );
    }

    /// A name that resolves to several addresses is walked on retry:
    /// the first attempt dials the first address, the next one dials
    /// the next.
    #[tokio::test]
    async fn a_name_with_several_addresses_is_walked_on_retry() {
        let lookup = CountingLookup::new(vec![
            "203.0.113.7".parse().unwrap(),
            "203.0.113.8".parse().unwrap(),
        ]);
        let resolver = Arc::new(BackendResolver::new(lookup));

        let first = resolver.address("api.internal", 80, 0).await.unwrap();
        let second = resolver.address("api.internal", 80, 1).await.unwrap();
        let third = resolver.address("api.internal", 80, 2).await.unwrap();

        assert_eq!(first.ip().to_string(), "203.0.113.7");
        assert_eq!(second.ip().to_string(), "203.0.113.8");
        assert_eq!(third.ip().to_string(), "203.0.113.7");
    }

    /// An IP literal is parsed, never resolved: a backend addressed
    /// by an address must not cost a DNS query.
    #[tokio::test]
    async fn an_ip_literal_is_never_resolved() {
        let lookup = CountingLookup::new(vec!["203.0.113.7".parse().unwrap()]);
        let resolver = Arc::new(BackendResolver::new(lookup.clone()));

        let addr = resolver.address("10.0.0.1", 8080, 0).await.unwrap();
        assert_eq!(addr, "10.0.0.1:8080".parse::<SocketAddr>().unwrap());
        assert_eq!(lookup.calls(), 0, "an address is not a name");
    }

    /// An IPv6 literal keeps its address family — the peer has to
    /// carry the brackets' contents, not `::1:8080`.
    #[tokio::test]
    async fn an_ipv6_literal_keeps_its_address_family() {
        let lookup = CountingLookup::new(vec!["203.0.113.7".parse().unwrap()]);
        let resolver = Arc::new(BackendResolver::new(lookup.clone()));

        let addr = resolver.address("::1", 8080, 0).await.unwrap();
        assert_eq!(addr, "[::1]:8080".parse::<SocketAddr>().unwrap());
        assert!(addr.is_ipv6());
        assert_eq!(lookup.calls(), 0);
    }

    /// A name that does not resolve is an **error**, never a panic:
    /// the caller answers 502.
    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_an_error() {
        struct Failing;
        #[async_trait]
        impl BackendLookup for Failing {
            async fn lookup(&self, _host: &str) -> Result<(Vec<IpAddr>, Duration), String> {
                Err("NXDOMAIN".to_string())
            }
        }
        let resolver = Arc::new(BackendResolver::new(Arc::new(Failing)));

        let err = resolver
            .address("nowhere.invalid", 80, 0)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolveError::Failed { .. }), "{err}");
        assert!(err.to_string().contains("nowhere.invalid"), "{err}");
    }

    /// A stale entry is served while it is refreshed in the
    /// background (issue #253).
    ///
    /// The request does not wait for the resolver: the old addresses
    /// stay in force until the refresh publishes new ones, so a slow
    /// nameserver never delays a request.
    #[tokio::test]
    async fn a_stale_entry_is_served_while_it_is_refreshed() {
        let lookup = CountingLookup::new(vec!["203.0.113.7".parse().unwrap()]);
        let resolver = Arc::new(BackendResolver::new(lookup.clone()));

        // The first request resolves the name.
        let addr = resolver.address("api.internal", 80, 0).await.unwrap();
        assert_eq!(addr.ip().to_string(), "203.0.113.7");
        assert_eq!(lookup.calls(), 1);

        // The entry goes stale.
        resolver.expire(&BackendKey {
            host: "api.internal".to_string(),
            port: 80,
        });

        // The next request is served the **stale** address …
        let addr = resolver.address("api.internal", 80, 0).await.unwrap();
        assert_eq!(
            addr.ip().to_string(),
            "203.0.113.7",
            "a stale entry keeps serving the old addresses"
        );
        assert_eq!(
            lookup.calls(),
            1,
            "the request must not wait for the resolver"
        );

        // … and the refresh happens in the background.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while lookup.calls() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            lookup.calls(),
            2,
            "the stale entry must be refreshed in the background"
        );
    }

    /// A failure is **shared** with everyone who waited for the
    /// attempt (issue #253).
    ///
    /// The waiters must not each resolve the name again: a backend
    /// that does not resolve would make every concurrent request pay
    /// the resolver's timeout in turn, so the tail of a burst waited
    /// N times as long as the first request.
    #[tokio::test]
    async fn a_failure_is_shared_with_every_waiter() {
        /// A lookup that counts its calls, waits a little and fails.
        struct SlowFailure {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl BackendLookup for SlowFailure {
            async fn lookup(&self, _host: &str) -> Result<(Vec<IpAddr>, Duration), String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err("NXDOMAIN".to_string())
            }
        }

        let lookup = Arc::new(SlowFailure {
            calls: AtomicUsize::new(0),
        });
        let resolver = Arc::new(BackendResolver::new(lookup.clone()));

        // Ten concurrent requests for a backend that does not resolve.
        let requests: Vec<_> = (0..10)
            .map(|_| resolver.address("api.internal", 80, 0))
            .collect();
        let results = futures_util::future::join_all(requests).await;

        assert!(
            results.iter().all(|r| r.is_err()),
            "every request must fail"
        );
        assert_eq!(
            lookup.calls.load(Ordering::SeqCst),
            1,
            "the waiters must share the one attempt, not each resolve again"
        );
    }

    /// A failed resolution is not cached: the next request tries
    /// again, so a name that starts resolving is picked up without a
    /// restart.
    #[tokio::test]
    async fn a_failed_resolution_is_not_cached() {
        struct Flaky {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl BackendLookup for Flaky {
            async fn lookup(&self, _host: &str) -> Result<(Vec<IpAddr>, Duration), String> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err("NXDOMAIN".to_string());
                }
                Ok((
                    vec!["203.0.113.7".parse().unwrap()],
                    Duration::from_secs(60),
                ))
            }
        }
        let resolver = Arc::new(BackendResolver::new(Arc::new(Flaky {
            calls: AtomicUsize::new(0),
        })));

        assert!(resolver.address("api.internal", 80, 0).await.is_err());
        let addr = resolver.address("api.internal", 80, 0).await.unwrap();
        assert_eq!(addr.ip().to_string(), "203.0.113.7");
    }

    /// The resolver's configuration comes from the system's
    /// `/etc/resolv.conf` — a backend is often an internal name the
    /// public resolvers cannot answer (issue #253).
    ///
    /// `ResolverConfig::default()` carries **no** nameservers, so a
    /// resolver built from it cannot answer anything.
    #[test]
    fn the_resolver_configuration_comes_from_the_system() {
        let (config, _opts) = read_system_conf().expect("the system has a resolv.conf");
        assert!(
            !config.name_servers.is_empty(),
            "the system configuration must name at least one nameserver"
        );
    }
}
