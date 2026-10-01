//! Process-wide stale-while-revalidate DNS cache for every reqwest client
//! in the app (API, images, trickplay).
//!
//! Resolving the server's bare hostname through the system resolver
//! intermittently stalls ~5s, while direct queries to the DNS server itself
//! answer in milliseconds; the OS-level fix (FQDN/hosts entry) is outside
//! the app, so this cache avoids paying the stall more than once:
//!
//! - First-ever lookup for an unseeded host blocks. A host seeded via
//!   [`CachingResolver::seed`] from a previous launch's persisted answer
//!   never blocks.
//! - Every later request is served from cache immediately; past
//!   [`REFRESH_AFTER`] a hit also kicks one background refresh
//!   (stale-while-revalidate).
//! - A failed refresh keeps the stale answer (serve-stale-on-error): for
//!   this app's single media server, a stale IP that no longer answers just
//!   surfaces as the connect error it is, and a resolver hiccup can't take
//!   down an otherwise-working session.
//!
//! One shared instance ([`shared_dns_resolver`]) backs all clients.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Age past which a cache hit also triggers a background refresh. Generous
/// because the target is a personal media server on a stable address --
/// freshness here only bounds how long a *moved* server keeps failing.
const REFRESH_AFTER: Duration = Duration::from_secs(5 * 60);

/// Blocking-thread lookup through the system resolver -- the same call
/// reqwest's default `GaiResolver` makes (port 0 is a placeholder; the
/// connector substitutes the real port).
fn system_lookup(host: &str) -> std::io::Result<Vec<SocketAddr>> {
    (host, 0).to_socket_addrs().map(Iterator::collect)
}

type LookupFn = dyn Fn(&str) -> std::io::Result<Vec<SocketAddr>> + Send + Sync;

/// Cap on how long one background refresh may stay "in flight" before a
/// later hit is allowed to launch a replacement: a `getaddrinfo` that never
/// returns would otherwise pin the in-flight marker forever, disabling
/// refresh for that host for the rest of the process. Well above any
/// observed stall (~5s), well below "forever".
const REFRESH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

struct CacheEntry {
    addrs: Vec<SocketAddr>,
    resolved_at: Instant,
    /// When the currently in-flight background refresh began -- `None` when
    /// no refresh is running. A burst of stale hits spawns one refresh, not
    /// one per request; a refresh older than [`REFRESH_ATTEMPT_TIMEOUT`] is
    /// treated as lost and a new one may start.
    refresh_started: Option<Instant>,
    /// Set only by [`CachingResolver::seed`]: this entry came off disk, not
    /// from this process's resolver.
    ///
    /// Makes the entry **born stale** without backdating `resolved_at`
    /// (`Instant` has no portable "5 minutes ago"), so the first hit serves
    /// it instantly and kicks the one background refresh that revalidates
    /// it. Cleared once that refresh launches, so a seeded host that can't
    /// be re-resolved falls back to ordinary [`REFRESH_AFTER`] pacing.
    seeded: bool,
}

pub struct CachingResolver {
    cache: Mutex<HashMap<String, CacheEntry>>,
    lookup: Arc<LookupFn>,
}

impl CachingResolver {
    fn new(lookup: Arc<LookupFn>) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            lookup,
        }
    }

    /// Pure peek at the cached first address for `host` -- no lookup, no
    /// refresh trigger. `None` until some HTTP client has resolved the host.
    fn cached_addr(&self, host: &str) -> Option<SocketAddr> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(host)
            .and_then(|entry| entry.addrs.first().copied())
    }

    /// Plant a previous launch's answer for `host` so the first lookup of
    /// this process doesn't have to wait on the system resolver (the stall
    /// this module documents is paid on the first lookup; unseeded, it
    /// blocks every request behind it).
    ///
    /// Inserted [`CacheEntry::seeded`] -- served instantly but revalidated
    /// in the background, since a seeded address is only ever a guess.
    /// Never blocks, never fails, and never overwrites an entry this
    /// process already resolved for real. Empty `addrs` is a no-op.
    pub fn seed(&self, host: &str, addrs: Vec<IpAddr>) {
        if addrs.is_empty() {
            return;
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.contains_key(host) {
            return;
        }
        cache.insert(
            host.to_string(),
            CacheEntry {
                addrs: addrs.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect(),
                resolved_at: Instant::now(),
                refresh_started: None,
                seeded: true,
            },
        );
        tracing::debug!(host, "dns cache seeded from a previous launch");
    }

    /// The addresses a request for `host` would be served right now, for
    /// persisting back to disk (the counterpart of [`Self::seed`]). `None`
    /// until something has resolved -- or seeded -- the host. Pure peek: no
    /// lookup, no refresh trigger.
    pub fn snapshot(&self, host: &str) -> Option<Vec<IpAddr>> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let entry = cache.get(host)?;
        Some(entry.addrs.iter().map(SocketAddr::ip).collect())
    }

    /// Cache-first resolution for `host`. Only the first-ever lookup for an
    /// unseeded host awaits the system resolver; everything after is an
    /// immediate cache hit (plus, past [`REFRESH_AFTER`] -- or on the first
    /// hit of a seeded entry -- a detached refresh).
    async fn cached_addrs(self: Arc<Self>, host: String) -> std::io::Result<Vec<SocketAddr>> {
        let cached = {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            match cache.get_mut(&host) {
                Some(entry) => {
                    let refresh_available = match entry.refresh_started {
                        None => true,
                        // A refresh this old is presumed lost (hung
                        // getaddrinfo); let a new one replace it. If the
                        // old one does eventually land, `store` just
                        // overwrites with equally-fresh data.
                        Some(started) => started.elapsed() > REFRESH_ATTEMPT_TIMEOUT,
                    };
                    // A seeded entry is born stale: serve it, and revalidate
                    // it on this very first hit (see `CacheEntry::seeded`).
                    let stale = entry.seeded || entry.resolved_at.elapsed() > REFRESH_AFTER;
                    let needs_refresh = stale && refresh_available;
                    if needs_refresh {
                        entry.refresh_started = Some(Instant::now());
                        entry.seeded = false;
                    }
                    Some((entry.addrs.clone(), needs_refresh))
                }
                None => None,
            }
        };
        if let Some((addrs, needs_refresh)) = cached {
            if needs_refresh {
                self.clone().spawn_refresh(host);
            }
            return Ok(addrs);
        }

        // First-ever lookup: nothing to serve yet, so this one waits (and
        // may eat the system resolver's stall -- once).
        let this = self.clone();
        let lookup_host = host.clone();
        let result = tokio::task::spawn_blocking(move || (this.lookup)(&lookup_host))
            .await
            .map_err(|e| std::io::Error::other(format!("dns lookup task failed: {e}")))?;
        if let Ok(addrs) = &result {
            self.store(host, addrs.clone());
        }
        result
    }

    fn store(&self, host: String, addrs: Vec<SocketAddr>) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            host,
            CacheEntry {
                addrs,
                resolved_at: Instant::now(),
                refresh_started: None,
                // A real answer from this process's resolver -- no longer a
                // guess off disk, whatever this entry used to be.
                seeded: false,
            },
        );
    }

    fn clear_refreshing(&self, host: &str) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get_mut(host) {
            entry.refresh_started = None;
        }
    }

    /// Detached refresh: success replaces the entry; failure keeps the
    /// stale answer (see the module docs). `try_current` guard: `resolve`
    /// is only ever polled from inside the client's tokio runtime, but a
    /// missing runtime must degrade to "skip the refresh", never a panic.
    fn spawn_refresh(self: Arc<Self>, host: String) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.clear_refreshing(&host);
            return;
        };
        handle.spawn(async move {
            let this = self.clone();
            let lookup_host = host.clone();
            let result =
                tokio::task::spawn_blocking(move || (this.lookup)(&lookup_host)).await;
            match result {
                Ok(Ok(addrs)) => self.store(host, addrs),
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, host = %host, "dns refresh failed; serving stale");
                    self.clear_refreshing(&host);
                }
                Err(e) => {
                    tracing::debug!(error = %e, host = %host, "dns refresh task failed; serving stale");
                    self.clear_refreshing(&host);
                }
            }
        });
    }
}

/// `Resolve`-implementing handle around a [`CachingResolver`]. A separate
/// type because `Resolve::resolve` takes `&self` but must return a
/// `'static` future -- the handle owns an `Arc` it can clone into the
/// future, without smuggling in a global.
pub struct ResolverHandle(Arc<CachingResolver>);

impl ResolverHandle {
    pub fn seed(&self, host: &str, addrs: Vec<IpAddr>) {
        self.0.seed(host, addrs);
    }

    pub fn snapshot(&self, host: &str) -> Option<Vec<IpAddr>> {
        self.0.snapshot(host)
    }
}

impl Resolve for ResolverHandle {
    fn resolve(&self, name: Name) -> Resolving {
        let this = self.0.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = this.cached_addrs(host).await?;
            let boxed: Addrs = Box::new(addrs.into_iter());
            Ok(boxed)
        })
    }
}

/// Rewrites a stream URL's hostname to the cached literal IP so mpv --
/// which resolves stream URLs with its own `getaddrinfo`, bypassing the
/// reqwest cache above -- never pays the system resolver's stall.
///
/// Plain-`http` only: an `https` URL keeps its hostname for SNI and
/// certificate validation. Already-literal-IP hosts, unknown hosts (cache
/// cold), and unparseable URLs pass through unchanged -- an optimization,
/// never a gate.
///
/// When rewritten, `host_header` carries the original authority (`host` or
/// `host:port`) and the caller MUST send it as an explicit `Host` header:
/// the IP is a transport detail, and a name-based reverse proxy in front of
/// plain-HTTP Jellyfin must keep seeing the hostname it routes by.
pub struct StreamTarget {
    pub url: String,
    /// `Some(original-authority)` exactly when `url` was rewritten to an IP.
    pub host_header: Option<String>,
}

pub fn rewrite_http_host_to_cached_ip(url_str: &str) -> StreamTarget {
    rewrite_with(&shared_dns_resolver().0, url_str)
}

fn rewrite_with(resolver: &CachingResolver, url_str: &str) -> StreamTarget {
    let unchanged = || StreamTarget {
        url: url_str.to_string(),
        host_header: None,
    };
    let Ok(mut parsed) = url::Url::parse(url_str) else {
        return unchanged();
    };
    if parsed.scheme() != "http" {
        return unchanged();
    }
    let Some(url::Host::Domain(domain)) = parsed.host() else {
        return unchanged(); // no host, or already a literal IP
    };
    let domain = domain.to_string();
    let Some(addr) = resolver.cached_addr(&domain) else {
        return unchanged();
    };
    // `Url::port()` is `Some` only for a non-default port -- exactly the
    // cases where the Host header carries one.
    let authority = match parsed.port() {
        Some(port) => format!("{domain}:{port}"),
        None => domain.clone(),
    };
    if parsed.set_ip_host(addr.ip()).is_err() {
        return unchanged();
    }
    tracing::debug!(host = %domain, ip = %addr.ip(), "stream url host replaced with cached ip");
    StreamTarget {
        url: parsed.to_string(),
        host_header: Some(authority),
    }
}

/// The hostname in `url` that's worth caching/persisting a DNS answer for:
/// a real domain name, never a literal IP (nothing to resolve) and never a
/// URL without a host. The one place callers -- e.g. the app's persisted
/// DNS seed, keyed by host -- turn a server `base_url` into that key, so
/// "is this even a name?" is decided identically everywhere.
pub fn cacheable_host(url: &str) -> Option<String> {
    match url::Url::parse(url).ok()?.host()? {
        url::Host::Domain(domain) => Some(domain.to_string()),
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => None,
    }
}

/// The process-wide resolver every client should pass to
/// `ClientBuilder::dns_resolver` -- one cache, shared by API, image, and
/// trickplay clients (`Arc<ResolverHandle>` coerces to `Arc<dyn Resolve>`).
pub fn shared_dns_resolver() -> Arc<ResolverHandle> {
    static RESOLVER: OnceLock<Arc<ResolverHandle>> = OnceLock::new();
    RESOLVER
        .get_or_init(|| {
            Arc::new(ResolverHandle(Arc::new(CachingResolver::new(Arc::new(
                system_lookup,
            )))))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, n], 0))
    }

    fn counting_resolver(fail: bool, counter: Arc<AtomicUsize>) -> Arc<CachingResolver> {
        Arc::new(CachingResolver::new(Arc::new(move |_host: &str| {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if fail && n > 1 {
                Err(std::io::Error::other("simulated resolver failure"))
            } else {
                Ok(vec![addr(n as u8)])
            }
        })))
    }

    #[test]
    fn rewrite_swaps_a_cached_http_host_for_its_ip_and_leaves_everything_else() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = counting_resolver(false, count.clone());
        resolver.store("storeserver".into(), vec![addr(9)]);

        // Cached http host: rewritten; Host header carries the authority.
        let hit = rewrite_with(
            &resolver,
            "http://storeserver:8096/Videos/abc/stream?ApiKey=t",
        );
        assert_eq!(hit.url, "http://127.0.0.9:8096/Videos/abc/stream?ApiKey=t");
        assert_eq!(hit.host_header.as_deref(), Some("storeserver:8096"));

        // Default port: Host header omits it, per convention.
        resolver.store("plain".into(), vec![addr(9)]);
        let default_port = rewrite_with(&resolver, "http://plain/x");
        assert_eq!(default_port.url, "http://127.0.0.9/x");
        assert_eq!(default_port.host_header.as_deref(), Some("plain"));

        // https keeps its hostname (SNI/cert validation) -- no header.
        let https = rewrite_with(&resolver, "https://storeserver:8096/x");
        assert_eq!(https.url, "https://storeserver:8096/x");
        assert_eq!(https.host_header, None);

        // Unknown host: cache cold, untouched.
        let cold = rewrite_with(&resolver, "http://elsewhere:8096/x");
        assert_eq!(cold.url, "http://elsewhere:8096/x");
        assert_eq!(cold.host_header, None);

        // Already a literal IP: untouched.
        let ip = rewrite_with(&resolver, "http://198.51.100.5:8096/x");
        assert_eq!(ip.url, "http://198.51.100.5:8096/x");
        assert_eq!(ip.host_header, None);
    }

    #[tokio::test]
    async fn a_lost_refresh_does_not_permanently_disable_refreshing() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = counting_resolver(false, count.clone());
        resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("prime");
        {
            let mut cache = resolver.cache.lock().expect("lock");
            let entry = cache.get_mut("server").expect("entry");
            // Refresh started long ago and never landed.
            entry.resolved_at = Instant::now() - REFRESH_AFTER - Duration::from_secs(1);
            entry.refresh_started =
                Some(Instant::now() - REFRESH_ATTEMPT_TIMEOUT - Duration::from_secs(1));
        }

        // A new hit must treat the old refresh as lost and launch another.
        resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("stale hit");
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if count.load(Ordering::SeqCst) >= 2 {
                break;
            }
        }
        assert!(
            count.load(Ordering::SeqCst) >= 2,
            "a replacement refresh must run after the timeout"
        );
    }

    #[tokio::test]
    async fn second_lookup_is_served_from_cache_without_a_resolver_call() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = counting_resolver(false, count.clone());

        let first = resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("first lookup");
        let second = resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("second lookup");

        assert_eq!(first, second);
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "a fresh cache hit must not call the system resolver again"
        );
    }

    #[tokio::test]
    async fn stale_hit_serves_immediately_and_refreshes_in_background() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = counting_resolver(false, count.clone());

        resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("prime");
        // Age the entry past REFRESH_AFTER without waiting real time.
        {
            let mut cache = resolver.cache.lock().expect("lock");
            cache.get_mut("server").expect("entry").resolved_at =
                Instant::now() - REFRESH_AFTER - Duration::from_secs(1);
        }

        let stale = resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("stale hit");
        assert_eq!(
            stale,
            vec![addr(1)],
            "stale hit must serve the cached answer"
        );

        // Poll the cache itself: the counter increments on the blocking
        // thread before the async task stores the result, so it can't be
        // the wait condition.
        let mut refreshed = Vec::new();
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            refreshed = resolver
                .clone()
                .cached_addrs("server".into())
                .await
                .expect("post-refresh hit");
            if refreshed == vec![addr(2)] {
                break;
            }
        }
        assert_eq!(refreshed, vec![addr(2)], "refresh must replace the entry");
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "exactly one background refresh must run"
        );
    }

    /// A seeded host must be served without ever touching the system
    /// resolver on the request path, and must still revalidate itself in
    /// the background on the first hit.
    #[tokio::test]
    async fn a_seeded_host_serves_instantly_and_revalidates_in_the_background() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = counting_resolver(false, count.clone());
        resolver.seed("server", vec![addr(7).ip()]);

        let first = resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("seeded hit");
        assert_eq!(
            first,
            vec![addr(7)],
            "a seeded host must be served from the seed, not resolved"
        );

        // Poll the cache itself (same ordering reasoning as the stale-hit
        // test above).
        let mut refreshed = Vec::new();
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            refreshed = resolver
                .clone()
                .cached_addrs("server".into())
                .await
                .expect("post-refresh hit");
            if refreshed == vec![addr(1)] {
                break;
            }
        }
        assert_eq!(
            refreshed,
            vec![addr(1)],
            "the seeded entry must be revalidated by a background refresh"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "exactly one background refresh must run -- and never a blocking \
             first lookup"
        );
        // Post-refresh the entry is an ordinary fresh one again: no further
        // resolver traffic until REFRESH_AFTER elapses.
        resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("fresh hit");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// `snapshot` is what the app persists back to disk, and `seed` must
    /// never overwrite an answer this process actually resolved (a live
    /// answer outranks one off disk).
    #[tokio::test]
    async fn snapshot_reports_the_served_addrs_and_seed_never_clobbers_a_live_one() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = counting_resolver(false, count.clone());
        assert_eq!(resolver.snapshot("server"), None, "nothing cached yet");

        resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("prime");
        assert_eq!(resolver.snapshot("server"), Some(vec![addr(1).ip()]));

        // A seed arriving after a real lookup is ignored...
        resolver.seed("server", vec![addr(7).ip()]);
        assert_eq!(resolver.snapshot("server"), Some(vec![addr(1).ip()]));
        // ...and an empty seed is a no-op rather than a poisoned entry.
        resolver.seed("elsewhere", vec![]);
        assert_eq!(resolver.snapshot("elsewhere"), None);
    }

    #[test]
    fn cacheable_host_accepts_names_and_rejects_ip_literals() {
        assert_eq!(
            cacheable_host("http://storeserver:8096"),
            Some("storeserver".to_string())
        );
        assert_eq!(
            cacheable_host("https://media.example.com/"),
            Some("media.example.com".to_string())
        );
        assert_eq!(cacheable_host("http://198.51.100.5:8096"), None);
        assert_eq!(cacheable_host("http://[::1]:8096"), None);
        assert_eq!(cacheable_host("not a url"), None);
    }

    #[tokio::test]
    async fn failed_refresh_keeps_serving_the_stale_answer() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = counting_resolver(true, count.clone());

        resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("prime");
        {
            let mut cache = resolver.cache.lock().expect("lock");
            cache.get_mut("server").expect("entry").resolved_at =
                Instant::now() - REFRESH_AFTER - Duration::from_secs(1);
        }

        let stale = resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("stale hit");
        assert_eq!(stale, vec![addr(1)]);
        // Poll the in-flight marker itself (same ordering reasoning as
        // above; it's cleared after the failing lookup returns).
        let mut cleared = false;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            cleared = resolver
                .cache
                .lock()
                .expect("lock")
                .get("server")
                .expect("entry")
                .refresh_started
                .is_none();
            if cleared {
                break;
            }
        }
        assert!(cleared, "a failed refresh must clear the in-flight marker");
        let after = resolver
            .clone()
            .cached_addrs("server".into())
            .await
            .expect("post-failed-refresh hit");
        assert_eq!(after, vec![addr(1)], "serve-stale-on-error");
    }
}
