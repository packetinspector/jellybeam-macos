//! Disk+memory image cache per docs/DATA.md §3: tag-keyed self-invalidating disk
//! layout, memory LRU of encoded bytes, request coalescing, and
//! cancel-on-scroll via a generation counter.
//!
//! `image_lru` bookkeeping lives in its own tiny sqlite db under the images
//! directory: `ImageCache::new` is only handed a directory + client (no
//! `Mirror`/db handle), so it can't share the mirror's connection.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tokio::sync::{Notify, OnceCell};

use jellyfin_api::JellyfinClient;

use crate::{CacheError, ImageKind};

const DISK_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Hard ceiling on a single image/sprite-sheet response body: the disk/mem
/// budgets below bound what the cache keeps, but only after the fact, so
/// this stops one hostile response from buffering gigabytes first. 64 MiB is
/// far above any real poster/backdrop/trickplay sheet.
const IMAGE_BODY_CAP: usize = 64 * 1024 * 1024;
/// Prevents a slow-drip response from holding a connection and its growing
/// buffer open indefinitely. Same shape as
/// `jellyfin-api::build_http_client`'s constants, with a longer whole-request
/// budget because these bodies are legitimately large.
const IMAGE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const IMAGE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// A running byte counter with evict-until-under-budget, same shape as the
/// disk LRU above.
const MEM_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
/// See `ImageCacheState::net_semaphore`. 4 keeps a ~12Mbit link saturated
/// despite ~118ms per-request latency (4 x ~30KB webp posters pipeline
/// nicely) while individual images still complete quickly; a LAN is fast
/// enough either way for the difference not to matter.
const IMAGE_FETCH_CONCURRENCY: usize = 4;

pub(crate) type FetchFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send>>;

/// One in-flight (or just-completed, briefly) fetch's coalescing cell.
type FetchCell = OnceCell<Result<Arc<Vec<u8>>, String>>;

/// The network fetch is behind a trait so tests can inject a counting fake
/// instead of hitting real HTTP (coalescing tests need to observe "N
/// concurrent gets = 1 underlying fetch").
pub(crate) trait ImageFetcher: Send + Sync {
    fn fetch(&self, url: String) -> FetchFuture;
}

struct HttpFetcher {
    http: reqwest::Client,
}

impl ImageFetcher for HttpFetcher {
    fn fetch(&self, url: String) -> FetchFuture {
        let http = self.http.clone();
        Box::pin(async move {
            let resp = http.get(&url).send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("http status {}", resp.status()));
            }
            read_capped(resp, IMAGE_BODY_CAP).await
        })
    }
}

/// Reads a response body chunk by chunk, refusing to buffer more than `cap`
/// bytes -- `Response::bytes()` reads the whole body into memory first,
/// which is an unbounded allocation against a hostile/broken server. Stops
/// (and drops the connection) the moment the running total would exceed the
/// ceiling. `cap` is a parameter, not a hard-coded constant, so tests can
/// drive the boundary without moving 64 MiB across a socket.
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return Ok(body),
            Err(e) => return Err(e.to_string()),
        };
        if body.len() + chunk.len() > cap {
            tracing::warn!(
                cap,
                "image response body exceeded the size cap; aborting read"
            );
            return Err(format!("response body exceeded {cap} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
}

fn kind_str(kind: ImageKind) -> &'static str {
    match kind {
        ImageKind::Primary => "primary",
        ImageKind::Backdrop => "backdrop",
        ImageKind::Thumb => "thumb",
        ImageKind::Trickplay => "trickplay",
    }
}

fn to_api_kind(kind: ImageKind) -> Option<jellyfin_api::ImageKind> {
    match kind {
        ImageKind::Primary => Some(jellyfin_api::ImageKind::Primary),
        ImageKind::Backdrop => Some(jellyfin_api::ImageKind::Backdrop),
        ImageKind::Thumb => Some(jellyfin_api::ImageKind::Thumb),
        // Trickplay sheets are fetched by `crates/app/src/trickplay.rs`, not
        // through this cache.
        ImageKind::Trickplay => None,
    }
}

fn cache_key(item_id: &str, kind: ImageKind, tag: &str, max_width: u32) -> String {
    format!("{item_id}-{}-{tag}-{max_width}", kind_str(kind))
}

/// Rejects a hostile/malformed `item_id`/`tag` containing `/` or `..` at the
/// public `get()` boundary, before any key is built -- otherwise it could
/// escape the cache directory entirely (path traversal).
fn validate_key_component(s: &str) -> Result<(), CacheError> {
    if s.contains('/') || s.contains("..") {
        return Err(CacheError::InvalidKey(s.to_string()));
    }
    Ok(())
}

/// The on-disk filename for a cache key: `hex(sha256(key))[..32]` plus the
/// `.jpg` suffix every cached image already used. This is what actually
/// touches the filesystem now (via `dir.join(..)`) -- fixed-shape, ASCII
/// hex, no path separators possible by construction, so no amount of
/// attacker-chosen `item_id`/`tag` content can influence where the file
/// lands. The human-readable `key` from `cache_key` is kept only as the
/// in-memory/db lookup key (mem LRU, `inflight` coalescing map, `image_lru`
/// row id) -- never as a path component again.
fn disk_filename(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    // Manual hex encoding rather than a `LowerHex` format string: sha2's
    // digest output type doesn't implement it, and this has no dependency
    // on that either way.
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}.jpg", &hex[..32])
}

/// Tracks a running byte total alongside an *unbounded* `LruCache` (so the
/// crate, not the `lru` dependency, owns capacity policy) and evicts
/// oldest-first after every insert until back under `budget_bytes` -- byte-
/// capped rather than entry-capped, so a few huge trickplay sheets can't use
/// far more memory than many small thumbnails for the same entry count.
pub(crate) struct MemCache {
    entries: lru::LruCache<String, Arc<Vec<u8>>>,
    total_bytes: u64,
    budget_bytes: u64,
}

impl MemCache {
    fn new(budget_bytes: u64) -> Self {
        Self {
            entries: lru::LruCache::unbounded(),
            total_bytes: 0,
            budget_bytes,
        }
    }

    fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
        self.entries.get(key).cloned()
    }

    /// A single entry heavier than the whole budget is allowed in
    /// transiently but then immediately evicted by the loop below (nothing
    /// left to evict but itself) -- i.e. it simply doesn't get memory-cached,
    /// rather than permanently blowing the budget. Still fully served from
    /// disk/network on the next `get()`.
    fn put(&mut self, key: String, bytes: Arc<Vec<u8>>) {
        let new_len = bytes.len() as u64;
        if let Some(old) = self.entries.put(key, bytes) {
            self.total_bytes = self.total_bytes.saturating_sub(old.len() as u64);
        }
        self.total_bytes = self.total_bytes.saturating_add(new_len);

        while self.total_bytes > self.budget_bytes {
            let Some((_, evicted)) = self.entries.pop_lru() else {
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(evicted.len() as u64);
        }
    }
}

pub(crate) struct ImageCacheState {
    dir: PathBuf,
    client: JellyfinClient,
    fetcher: Arc<dyn ImageFetcher>,
    mem: Mutex<MemCache>,
    inflight: Mutex<HashMap<String, Arc<FetchCell>>>,
    lru_db: Arc<Mutex<Connection>>,
    generation: AtomicU64,
    notify: Notify,
    /// Bounds in-flight fetches: unbounded, a poster wall issues every
    /// visible cell's fetch at once, which on a slow link makes every image
    /// arrive at roughly the worst-case time. Bounding them keeps
    /// completions sequential-ish, keeps queued requests costless to cancel
    /// (they wait here, nothing on the wire), and opens fewer sockets.
    net_semaphore: tokio::sync::Semaphore,
    /// Counts every `get()` resolution by where it was served from, so a
    /// cold vs. warm launch can be compared concretely instead of eyeballed.
    /// Always-on (not feature-flagged): reading atomics costs nothing on
    /// the hot path.
    stats: CacheStats,
}

#[derive(Default)]
struct CacheStats {
    mem_hits: AtomicU64,
    disk_hits: AtomicU64,
    network_fetches: AtomicU64,
    network_errors: AtomicU64,
}

/// Snapshot of [`ImageCache`]'s hit/miss counters at the moment it was read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageCacheStats {
    /// Served from the in-process memory LRU (`MemCache`) without touching disk.
    pub mem_hits: u64,
    /// Not in memory, but found on disk (`image_lru.db`-tracked cache dir)
    /// without a network fetch.
    pub disk_hits: u64,
    /// Neither memory nor disk had it: an HTTP GET was actually issued.
    pub network_fetches: u64,
    /// Of `network_fetches`, how many came back as an error (not a cache
    /// miss by itself, but useful context alongside the other three).
    pub network_errors: u64,
}

impl ImageCacheStats {
    /// Total `get()` resolutions this snapshot covers (successful
    /// fetches only; `network_errors` is a subset of `network_fetches`,
    /// already included in `total()` -- not double-counted).
    pub fn total(&self) -> u64 {
        self.mem_hits + self.disk_hits + self.network_fetches
    }

    /// Fraction (0.0-1.0) served without a network round-trip. `None` if
    /// nothing has been requested yet (avoids a 0/0 division).
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        Some((self.mem_hits + self.disk_hits) as f64 / total as f64)
    }
}

fn db_err(e: impl std::fmt::Display) -> CacheError {
    CacheError::Db(e.to_string())
}

fn open_lru_db(dir: &Path) -> Result<Connection, CacheError> {
    let conn = Connection::open(dir.join("image_lru.db")).map_err(db_err)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS image_lru (
            key TEXT PRIMARY KEY,
            bytes INTEGER,
            last_access INTEGER
        );",
    )
    .map_err(db_err)?;
    Ok(conn)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Evict oldest-first (by `last_access`) until under `budget` bytes. Split
/// out as a free function over a plain `&Connection` + dir so it's directly
/// unit-testable without spinning up the whole `ImageCache`.
pub(crate) fn enforce_budget_blocking(conn: &Connection, dir: &Path, budget: u64) -> usize {
    let total: i64 = conn
        .query_row("SELECT COALESCE(SUM(bytes), 0) FROM image_lru", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if (total as u64) <= budget {
        return 0;
    }

    let mut to_free = total as u64 - budget;
    let mut evicted = 0usize;
    let victims: Vec<(String, i64)> = {
        let mut stmt =
            match conn.prepare("SELECT key, bytes FROM image_lru ORDER BY last_access ASC") {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "failed to prepare eviction scan");
                    return 0;
                }
            };
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        }
    };

    for (key, bytes) in victims {
        if to_free == 0 {
            break;
        }
        let _ = conn.execute("DELETE FROM image_lru WHERE key = ?1", [&key]);
        let _ = std::fs::remove_file(dir.join(disk_filename(&key)));
        to_free = to_free.saturating_sub(bytes.max(0) as u64);
        evicted += 1;
    }
    evicted
}

impl ImageCacheState {
    fn mem_get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let mut mem = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        mem.get(key)
    }

    fn mem_put(&self, key: &str, bytes: Arc<Vec<u8>>) {
        let mut mem = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        mem.put(key.to_string(), bytes);
    }

    async fn disk_get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let path = self.dir.join(disk_filename(key));
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(&path).ok())
            .await
            .ok()
            .flatten()?;
        self.touch_lru(key).await;
        Some(Arc::new(bytes))
    }

    async fn touch_lru(&self, key: &str) {
        let db = self.lru_db.clone();
        let key = key.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "UPDATE image_lru SET last_access = ?1 WHERE key = ?2",
                rusqlite::params![now_secs(), key],
            )
        })
        .await;
    }

    async fn store_to_disk_and_lru(&self, key: &str, bytes: Arc<Vec<u8>>) {
        let path = self.dir.join(disk_filename(key));
        let write_bytes = bytes.clone();
        let write_path = path.clone();
        // Must match all three arms of `Result<io::Result<()>, JoinError>`:
        // dropping the inner `fs::write` result would silently miss a
        // disk-full/permission failure while `image_lru` still bills `len`
        // bytes for data that was never written.
        match tokio::task::spawn_blocking(move || {
            std::fs::write(&write_path, write_bytes.as_slice())
        })
        .await
        {
            Err(e) => {
                tracing::error!(error = %e, "disk write task panicked");
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, ?path, "image cache disk write failed; not recording an LRU row");
                return;
            }
            Ok(Ok(())) => {}
        }

        let db = self.lru_db.clone();
        let key = key.to_string();
        let dir = self.dir.clone();
        let len = bytes.len() as i64;
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = conn.execute(
                "INSERT INTO image_lru (key, bytes, last_access) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET bytes = excluded.bytes, last_access = excluded.last_access",
                rusqlite::params![key, len, now_secs()],
            ) {
                tracing::error!(error = %e, "failed to record image_lru row");
            }
            enforce_budget_blocking(&conn, &dir, DISK_BUDGET_BYTES);
        })
        .await;
    }

    fn get_or_create_cell(&self, key: &str) -> Arc<FetchCell> {
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        inflight
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone()
    }

    fn drop_cell(&self, key: &str) {
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        inflight.remove(key);
    }

    /// Resolves once the priority floor has advanced past `my_generation`.
    async fn wait_cancelled(&self, my_generation: u64) {
        loop {
            let notified = self.notify.notified();
            if self.generation.load(Ordering::SeqCst) > my_generation {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone)]
pub struct ImageCache {
    pub(crate) inner: Arc<ImageCacheState>,
}

impl ImageCache {
    pub fn new(dir: std::path::PathBuf, client: JellyfinClient) -> Self {
        Self::with_fetcher(dir, client, |http| Arc::new(HttpFetcher { http }))
    }

    /// Test-only seam: build with an injected fetcher instead of real HTTP,
    /// so coalescing/cancellation tests can count/control fetches precisely.
    pub(crate) fn with_fetcher(
        dir: std::path::PathBuf,
        client: JellyfinClient,
        make_fetcher: impl FnOnce(reqwest::Client) -> Arc<dyn ImageFetcher>,
    ) -> Self {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!(error = %e, ?dir, "failed to create image cache directory");
        }
        let lru_db = match open_lru_db(&dir) {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!(error = %e, "failed to open image_lru db; falling back to in-memory (no disk persistence)");
                // An in-memory sqlite handle failing to open would mean
                // sqlite itself is unusable in this process -- every other
                // db in the app (the mirror itself) would already be dead,
                // so there is no graceful degradation left to attempt here.
                Connection::open_in_memory().expect("in-memory sqlite open should never fail")
            }
        };

        // `expect` matches `jellyfin-api::build_http_client`: the only
        // failure mode is the TLS backend failing to initialize, which
        // `Client::new()` already panicked on internally, so this is not a
        // new panic surface.
        let http = reqwest::Client::builder()
            .connect_timeout(IMAGE_CONNECT_TIMEOUT)
            .timeout(IMAGE_REQUEST_TIMEOUT)
            // Shared process-wide DNS cache: image fetches open the most
            // sockets of any client here, so they gain the most from never
            // re-paying the system resolver's intermittent ~5s stall (see
            // jellyfin_api::dns's module docs).
            .dns_resolver(jellyfin_api::dns::shared_dns_resolver())
            .build()
            .expect("reqwest client with connect/request timeouts should always build");
        let fetcher = make_fetcher(http);

        Self {
            inner: Arc::new(ImageCacheState {
                dir,
                client,
                fetcher,
                mem: Mutex::new(MemCache::new(MEM_BUDGET_BYTES)),
                inflight: Mutex::new(HashMap::new()),
                lru_db: Arc::new(Mutex::new(lru_db)),
                generation: AtomicU64::new(0),
                notify: Notify::new(),
                net_semaphore: tokio::sync::Semaphore::new(IMAGE_FETCH_CONCURRENCY),
                stats: CacheStats::default(),
            }),
        }
    }

    /// A snapshot of hit/miss counts since this `ImageCache` was constructed
    /// (or since the last [`ImageCache::reset_stats`]).
    pub fn stats(&self) -> ImageCacheStats {
        ImageCacheStats {
            mem_hits: self.inner.stats.mem_hits.load(Ordering::Relaxed),
            disk_hits: self.inner.stats.disk_hits.load(Ordering::Relaxed),
            network_fetches: self.inner.stats.network_fetches.load(Ordering::Relaxed),
            network_errors: self.inner.stats.network_errors.load(Ordering::Relaxed),
        }
    }

    /// Zeroes the counters `stats()` reports -- lets a loadtest harness mark
    /// "instrumentation starts here" (e.g. right after login, before
    /// measuring a specific window) without needing a fresh `ImageCache`.
    pub fn reset_stats(&self) {
        self.inner.stats.mem_hits.store(0, Ordering::Relaxed);
        self.inner.stats.disk_hits.store(0, Ordering::Relaxed);
        self.inner.stats.network_fetches.store(0, Ordering::Relaxed);
        self.inner.stats.network_errors.store(0, Ordering::Relaxed);
    }

    /// Cheap "is this exact key already on disk?" probe: one `stat` on the
    /// hashed filename (`disk_filename`), nothing else. Used by the
    /// background poster warmer (`app/src/image_warm.rs`) to probe before
    /// falling back to `get()`, avoiding reading every cached poster's bytes
    /// back off disk on every rescan.
    ///
    /// Deliberately does NOT touch the memory LRU, `image_lru.last_access`,
    /// or the `stats()` counters: a warm pass over an entire library must
    /// not evict the foreground's working set, flatten disk-LRU recency, or
    /// count as a `get()` resolution.
    ///
    /// Returns `false` for a key component `get()` would reject
    /// (`validate_key_component`); a `true` is not a hard promise a later
    /// `get()` avoids the network (the file could be evicted meanwhile).
    ///
    /// Synchronous and blocking (a single `stat`); batch calls inside one
    /// `spawn_blocking` rather than interleaving with `.await` points.
    pub fn is_cached_on_disk(
        &self,
        item_id: &str,
        kind: ImageKind,
        tag: &str,
        max_width: u32,
    ) -> bool {
        if validate_key_component(item_id).is_err() || validate_key_component(tag).is_err() {
            return false;
        }
        let key = cache_key(item_id, kind, tag, max_width);
        self.inner.dir.join(disk_filename(&key)).is_file()
    }

    pub async fn get(
        &self,
        item_id: &str,
        kind: ImageKind,
        tag: &str,
        max_width: u32,
    ) -> Result<Arc<Vec<u8>>, CacheError> {
        // Reject a hostile item_id/tag before it's anywhere near a path.
        validate_key_component(item_id)?;
        validate_key_component(tag)?;

        let key = cache_key(item_id, kind, tag, max_width);

        if let Some(bytes) = self.inner.mem_get(&key) {
            self.inner.stats.mem_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(bytes);
        }

        let my_generation = self.inner.generation.load(Ordering::SeqCst);

        if let Some(bytes) = self.inner.disk_get(&key).await {
            self.inner.mem_put(&key, bytes.clone());
            self.inner.stats.disk_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(bytes);
        }

        let Some(api_kind) = to_api_kind(kind) else {
            return Err(CacheError::Db(format!(
                "no URL builder for {kind:?} images (jellyfin-api's image_url only covers Primary/Backdrop/Thumb)"
            )));
        };
        let url = self
            .inner
            .client
            .image_url(item_id, api_kind, tag, max_width);

        let cell = self.inner.get_or_create_cell(&key);
        let state = self.inner.clone();
        let fetch_key = key.clone();
        let fetch = async {
            // Queue here, not on the wire (see `net_semaphore`). Waiting
            // inside the coalescing cell keeps a queued request fully
            // cancellable by the `select!` below at zero network cost.
            // `acquire` only errors after `Semaphore::close`, which nothing
            // ever calls on this semaphore.
            let permit = state
                .net_semaphore
                .acquire()
                .await
                .expect("image fetch semaphore is never closed");
            // Counted the instant the GET is actually issued (not after it
            // resolves) -- "network_fetches" must reflect real requests sent
            // over the wire, matching what a network inspector would show,
            // even if this particular fetch later loses the cancel-on-scroll
            // race in the `select!` below and never gets to store its
            // result.
            state.stats.network_fetches.fetch_add(1, Ordering::Relaxed);
            let fetched = state.fetcher.fetch(url).await;
            // Release before the disk write: the next queued fetch can use
            // the wire while this one persists locally.
            drop(permit);
            match fetched {
                Ok(bytes) => {
                    let arc = Arc::new(bytes);
                    state.store_to_disk_and_lru(&fetch_key, arc.clone()).await;
                    state.mem_put(&fetch_key, arc.clone());
                    Ok(arc)
                }
                Err(e) => {
                    state.stats.network_errors.fetch_add(1, Ordering::Relaxed);
                    Err(e)
                }
            }
        };

        // `tokio::sync::OnceCell::get_or_init` is cancel-safe: if this
        // future is dropped before the initializer completes (the
        // cancellation branch below wins the race), the cell is left
        // uninitialized and a later `get()` for the same key starts fresh.
        // That's exactly "offscreen requests are dropped, not queued" -- a
        // cancelled request doesn't leave a queued fetch behind.
        let result = tokio::select! {
            biased;
            _ = self.inner.wait_cancelled(my_generation) => Err(CacheError::Cancelled),
            r = cell.get_or_init(|| fetch) => r.clone().map_err(CacheError::Db),
        };

        self.inner.drop_cell(&key);
        result
    }

    /// See docs/DATA.md §3: "visible + 1 screen ahead keep priority; offscreen
    /// requests are dropped, not queued." `generation` is a monotonically
    /// increasing scroll/priority counter the UI advances as it scrolls;
    /// any `get()` calls issued at an older generation and still pending are
    /// woken up to bail out with `CacheError::Cancelled`.
    pub fn cancel_below_priority(&self, generation: u64) {
        self.inner
            .generation
            .fetch_max(generation, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jellyfin_api::ClientIdentity;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn identity() -> ClientIdentity {
        ClientIdentity {
            client: "t".to_string(),
            device: "t".to_string(),
            device_id: "t".to_string(),
            version: "0".to_string(),
        }
    }

    struct CountingFetcher {
        calls: Arc<AtomicUsize>,
        delay: Duration,
        payload: Vec<u8>,
    }

    impl ImageFetcher for CountingFetcher {
        fn fetch(&self, _url: String) -> FetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let delay = self.delay;
            let payload = self.payload.clone();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(payload)
            })
        }
    }

    struct FailingFetcher {
        calls: Arc<AtomicUsize>,
    }
    impl ImageFetcher for FailingFetcher {
        fn fetch(&self, _url: String) -> FetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Err("boom".to_string()) })
        }
    }

    fn test_cache(dir: &Path, fetcher: Arc<dyn ImageFetcher>) -> ImageCache {
        let client = JellyfinClient::from_token("http://localhost:8096", identity(), "tok");
        ImageCache::with_fetcher(dir.to_path_buf(), client, move |_http| fetcher)
    }

    #[tokio::test]
    async fn concurrent_gets_for_same_key_coalesce_into_one_fetch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(50),
            payload: vec![1, 2, 3],
        });
        let cache = test_cache(dir.path(), fetcher);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            handles.push(tokio::spawn(async move {
                cache.get("item1", ImageKind::Primary, "tag1", 300).await
            }));
        }
        for h in handles {
            let bytes = h.await.expect("join").expect("get succeeds");
            assert_eq!(*bytes, vec![1, 2, 3]);
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "8 concurrent requests for the same key must coalesce to 1 fetch"
        );
    }

    #[tokio::test]
    async fn different_tags_do_not_coalesce() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(5),
            payload: vec![9],
        });
        let cache = test_cache(dir.path(), fetcher);

        cache
            .get("item1", ImageKind::Primary, "tagA", 300)
            .await
            .expect("get a");
        cache
            .get("item1", ImageKind::Primary, "tagB", 300)
            .await
            .expect("get b");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a tag change is a new cache key -> separate fetch"
        );
    }

    #[tokio::test]
    async fn second_call_after_first_completes_is_served_from_memory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![7],
        });
        let cache = test_cache(dir.path(), fetcher);

        cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("first");
        cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("second");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "memory hit must not refetch"
        );
    }

    // --- hit/miss counters -------------------------------------------

    #[tokio::test]
    async fn stats_counts_a_cold_get_as_a_network_fetch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);

        cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("get");

        let stats = cache.stats();
        assert_eq!(stats.network_fetches, 1);
        assert_eq!(stats.mem_hits, 0);
        assert_eq!(stats.disk_hits, 0);
        assert_eq!(stats.total(), 1);
    }

    #[tokio::test]
    async fn stats_counts_a_repeat_get_as_a_mem_hit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);

        cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("first (network)");
        cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("second (mem hit)");

        let stats = cache.stats();
        assert_eq!(stats.network_fetches, 1);
        assert_eq!(stats.mem_hits, 1);
        assert_eq!(stats.total(), 2);
        assert_eq!(stats.hit_rate(), Some(0.5));
    }

    #[tokio::test]
    async fn stats_counts_a_disk_hit_on_a_fresh_process_instance() {
        // Pins: a fresh ImageCache reading disk populated by a prior
        // instance counts as a disk hit, not a network fetch.
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![4, 2],
        });
        {
            let cache = test_cache(dir.path(), fetcher.clone());
            cache
                .get("item1", ImageKind::Primary, "tag1", 300)
                .await
                .expect("populate");
        }

        let calls2 = Arc::new(AtomicUsize::new(0));
        let fetcher2 = Arc::new(CountingFetcher {
            calls: calls2.clone(),
            delay: Duration::from_millis(1),
            payload: vec![9, 9],
        });
        let cache2 = test_cache(dir.path(), fetcher2);
        cache2
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("disk hit");

        let stats = cache2.stats();
        assert_eq!(stats.disk_hits, 1);
        assert_eq!(stats.network_fetches, 0);
        assert_eq!(stats.hit_rate(), Some(1.0));
    }

    #[tokio::test]
    async fn stats_counts_network_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(FailingFetcher {
            calls: calls.clone(),
        });
        let cache = test_cache(dir.path(), fetcher);

        let _ = cache.get("item1", ImageKind::Primary, "tag1", 300).await;

        let stats = cache.stats();
        assert_eq!(stats.network_fetches, 1);
        assert_eq!(stats.network_errors, 1);
    }

    #[tokio::test]
    async fn reset_stats_zeroes_the_counters() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);
        cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("get");
        assert_eq!(cache.stats().total(), 1);

        cache.reset_stats();
        assert_eq!(cache.stats(), ImageCacheStats::default());
    }

    #[test]
    fn hit_rate_is_none_with_no_requests_yet() {
        assert_eq!(ImageCacheStats::default().hit_rate(), None);
    }

    #[tokio::test]
    async fn fresh_cache_reads_disk_populated_by_a_prior_instance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![4, 2],
        });
        {
            let cache = test_cache(dir.path(), fetcher.clone());
            cache
                .get("item1", ImageKind::Primary, "tag1", 300)
                .await
                .expect("populate");
        }

        let calls2 = Arc::new(AtomicUsize::new(0));
        let fetcher2 = Arc::new(CountingFetcher {
            calls: calls2.clone(),
            delay: Duration::from_millis(1),
            payload: vec![9, 9],
        });
        let cache2 = test_cache(dir.path(), fetcher2);
        let bytes = cache2
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("disk hit");

        assert_eq!(
            *bytes,
            vec![4, 2],
            "must come from disk, not the second fetcher's payload"
        );
        assert_eq!(
            calls2.load(Ordering::SeqCst),
            0,
            "disk hit must not touch the network"
        );
    }

    // --- background poster warming: the cheap disk-existence probe --------

    /// Pins: `is_cached_on_disk` reports a miss before any store and a hit
    /// after, without any of a real `get()`'s side effects. Checked via a
    /// fresh `ImageCache` for the "after" half so the memory LRU is provably
    /// empty going in.
    #[tokio::test]
    async fn is_cached_on_disk_reports_miss_then_hit_with_no_mem_cache_side_effects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![3, 1, 4],
        });
        let cache = test_cache(dir.path(), fetcher);

        assert!(
            !cache.is_cached_on_disk("item1", ImageKind::Primary, "tag1", 300),
            "nothing has been stored yet"
        );

        cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("populate");

        let calls2 = Arc::new(AtomicUsize::new(0));
        let fetcher2 = Arc::new(CountingFetcher {
            calls: calls2.clone(),
            delay: Duration::from_millis(1),
            payload: vec![0],
        });
        let fresh = test_cache(dir.path(), fetcher2);
        assert!(
            fresh.is_cached_on_disk("item1", ImageKind::Primary, "tag1", 300),
            "the stored key must probe as present on a fresh cache instance"
        );

        {
            let mem = fresh.inner.mem.lock().expect("mem lock");
            assert_eq!(
                mem.entries.len(),
                0,
                "the probe must not populate the memory LRU"
            );
            assert_eq!(mem.total_bytes, 0);
        }
        assert_eq!(
            fresh.stats(),
            ImageCacheStats::default(),
            "the probe is not a get() resolution and must not move any counter"
        );
        assert_eq!(
            calls2.load(Ordering::SeqCst),
            0,
            "the probe must never reach the network"
        );
    }

    /// Pins: every key component (tag/kind/width) participates -- a
    /// different `cache_key` probes as a miss, exactly like `get()`; a
    /// hostile component is rejected the same way `get()` rejects it.
    #[tokio::test]
    async fn is_cached_on_disk_is_keyed_by_every_component() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);
        cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("populate");

        assert!(cache.is_cached_on_disk("item1", ImageKind::Primary, "tag1", 300));
        assert!(!cache.is_cached_on_disk("item2", ImageKind::Primary, "tag1", 300));
        assert!(!cache.is_cached_on_disk("item1", ImageKind::Primary, "tag2", 300));
        assert!(!cache.is_cached_on_disk("item1", ImageKind::Thumb, "tag1", 300));
        assert!(!cache.is_cached_on_disk("item1", ImageKind::Primary, "tag1", 320));
        assert!(!cache.is_cached_on_disk("../escape", ImageKind::Primary, "tag1", 300));
        assert!(!cache.is_cached_on_disk("item1", ImageKind::Primary, "../../etc/passwd", 300));
    }

    #[tokio::test]
    async fn cancel_below_priority_cancels_a_still_pending_get() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(200),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);

        let cache2 = cache.clone();
        let handle =
            tokio::spawn(async move { cache2.get("item1", ImageKind::Primary, "tag1", 300).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cache.cancel_below_priority(1);

        let result = handle.await.expect("join");
        assert!(
            matches!(result, Err(CacheError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
    }

    #[tokio::test]
    async fn cancel_at_generation_zero_does_not_cancel_requests_issued_at_zero() {
        // A request captured at generation 0 must not be cancelled by a
        // cancel call that doesn't advance the floor past 0.
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(5),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);

        cache.cancel_below_priority(0);
        let result = cache.get("item1", ImageKind::Primary, "tag1", 300).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn fetch_error_propagates_and_does_not_poison_future_requests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(FailingFetcher {
            calls: calls.clone(),
        });
        let cache = test_cache(dir.path(), fetcher);

        let first = cache.get("item1", ImageKind::Primary, "tag1", 300).await;
        assert!(first.is_err());
        let second = cache.get("item1", ImageKind::Primary, "tag1", 300).await;
        assert!(second.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a failed fetch must not be permanently cached; retried on next get()"
        );
    }

    #[tokio::test]
    async fn hostile_item_id_is_rejected_and_writes_nothing_outside_the_cache_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: b"pwned".to_vec(),
        });
        let cache = test_cache(dir.path(), fetcher);

        // Where the pre-fix, vulnerable `cache_key`-as-filename scheme would
        // have written this: one directory above the cache dir.
        let escape_target = dir
            .path()
            .parent()
            .expect("tempdir has a parent")
            .join("escaped-item-primary-tag1-300.jpg");
        let _ = std::fs::remove_file(&escape_target); // in case a stale run left one

        let result = cache
            .get("../escaped-item", ImageKind::Primary, "tag1", 300)
            .await;
        assert!(
            matches!(result, Err(CacheError::InvalidKey(_))),
            "expected InvalidKey, got {result:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a rejected key must never reach the network fetch"
        );
        assert!(
            !escape_target.exists(),
            "nothing must be written outside the cache directory"
        );
    }

    #[tokio::test]
    async fn hostile_tag_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);

        let result = cache
            .get("item1", ImageKind::Primary, "../../etc/passwd", 300)
            .await;
        assert!(matches!(result, Err(CacheError::InvalidKey(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn slash_in_item_id_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);

        let result = cache.get("a/b", ImageKind::Primary, "tag1", 300).await;
        assert!(matches!(result, Err(CacheError::InvalidKey(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn well_behaved_item_id_and_tag_still_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![42],
        });
        let cache = test_cache(dir.path(), fetcher);

        let result = cache
            .get("item-1", ImageKind::Primary, "etag-value", 300)
            .await;
        assert_eq!(*result.expect("get succeeds"), vec![42]);
    }

    #[test]
    fn disk_filename_has_no_path_separators_for_a_hostile_key() {
        let name = disk_filename("../../../etc/passwd-primary-tag-300");
        assert!(!name.contains('/'));
        assert!(!name.contains(".."));
        assert!(name.ends_with(".jpg"));
    }

    /// `store_to_disk_and_lru`'s
    /// `if let Err(e) = spawn_blocking(|| std::fs::write(..)).await` used to
    /// inspect only the `JoinError` -- the inner `std::io::Result` the
    /// blocking closure returns was dropped on the `Ok(_)` arm, so a
    /// genuinely failed disk write went unlogged AND still got an
    /// `image_lru` row claiming those bytes were on disk, making the
    /// disk-budget accounting over-count by the size of every failed write
    /// (and evict real entries to make room for bytes that never landed).
    /// Fixed by matching all three arms; this test was written against the
    /// old behavior and is inverted here, per the finding's own note, to
    /// assert the ledger stays empty.
    ///
    /// Forced here by pre-creating a *directory* at the exact path
    /// `disk_filename` resolves to, making `std::fs::write` fail with
    /// `EISDIR` without touching any global state.
    #[tokio::test]
    async fn failed_disk_write_is_not_recorded_in_the_lru_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = cache_key("item1", ImageKind::Primary, "tag1", 300);
        // Make the eventual `std::fs::write` target un-writable.
        std::fs::create_dir_all(dir.path().join(disk_filename(&key))).expect("blocking dir");

        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![7; 4096],
        });
        let cache = test_cache(dir.path(), fetcher);

        let bytes = cache
            .get("item1", ImageKind::Primary, "tag1", 300)
            .await
            .expect("get still succeeds -- the write failure is not surfaced");
        assert_eq!(bytes.len(), 4096);

        // The blob is definitively not on disk (it's a directory), so the
        // ledger must not claim any bytes for this key.
        assert!(dir.path().join(disk_filename(&key)).is_dir());
        let ledger = Connection::open(dir.path().join("image_lru.db")).expect("open ledger");
        let recorded: Option<i64> = ledger
            .query_row("SELECT bytes FROM image_lru WHERE key = ?1", [&key], |r| {
                r.get(0)
            })
            .ok();
        assert_eq!(
            recorded, None,
            "a write that never landed must not be billed to the disk budget"
        );
    }

    #[tokio::test]
    async fn trickplay_kind_returns_error_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = Arc::new(CountingFetcher {
            calls: calls.clone(),
            delay: Duration::from_millis(1),
            payload: vec![1],
        });
        let cache = test_cache(dir.path(), fetcher);
        let result = cache.get("item1", ImageKind::Trickplay, "tag1", 300).await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn eviction_respects_budget_oldest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_lru_db(dir.path()).expect("open");

        // On-disk filenames are the hashed form, not the
        // raw key -- `image_lru.key` still stores the human-readable key.
        for (key, bytes, access) in [("a", 100i64, 1i64), ("b", 100, 2), ("c", 100, 3)] {
            conn.execute(
                "INSERT INTO image_lru (key, bytes, last_access) VALUES (?1, ?2, ?3)",
                rusqlite::params![key, bytes, access],
            )
            .expect("insert");
            std::fs::write(dir.path().join(disk_filename(key)), b"x").expect("write");
        }

        let evicted = enforce_budget_blocking(&conn, dir.path(), 150);
        assert_eq!(evicted, 2, "must evict oldest entries until under budget");

        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT key FROM image_lru ORDER BY key")
                .expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("rows")
        };
        assert_eq!(
            remaining,
            vec!["c".to_string()],
            "newest-accessed entry must survive"
        );
        assert!(!dir.path().join(disk_filename("a")).exists());
        assert!(!dir.path().join(disk_filename("b")).exists());
        assert!(dir.path().join(disk_filename("c")).exists());
    }

    #[test]
    fn eviction_is_a_noop_under_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_lru_db(dir.path()).expect("open");
        conn.execute(
            "INSERT INTO image_lru (key, bytes, last_access) VALUES ('a', 10, 1)",
            [],
        )
        .expect("insert");
        assert_eq!(enforce_budget_blocking(&conn, dir.path(), 1_000_000), 0);
    }

    // --- byte-budget memory LRU --------------------------

    #[test]
    fn mem_cache_evicts_oldest_until_under_byte_budget() {
        let mut mem = MemCache::new(250);
        mem.put("a".to_string(), Arc::new(vec![0u8; 100]));
        mem.put("b".to_string(), Arc::new(vec![0u8; 100]));
        assert!(mem.get("a").is_some());

        // Total would be 300 > 250: the least-recently-used entry ("b",
        // since "a" was just touched by the `get` above) must go.
        mem.put("c".to_string(), Arc::new(vec![0u8; 100]));
        assert!(mem.get("a").is_some(), "recently-touched entry survives");
        assert!(mem.get("b").is_none(), "least-recently-used entry evicted");
        assert!(mem.get("c").is_some());
    }

    #[test]
    fn mem_cache_is_byte_capped_not_entry_capped() {
        // Many small entries can coexist even though the old entry-capped
        // LRU would have started evicting long before this many were added.
        let mut mem = MemCache::new(10_000);
        for i in 0..1000 {
            mem.put(format!("key-{i}"), Arc::new(vec![0u8; 1]));
        }
        assert!(
            mem.get("key-999").is_some(),
            "1000 one-byte entries is well under a 10KB budget"
        );

        // But one entry far bigger than the budget must not be allowed to
        // balloon total usage past it.
        let mut mem = MemCache::new(1_000);
        mem.put("small".to_string(), Arc::new(vec![0u8; 10]));
        mem.put("huge".to_string(), Arc::new(vec![0u8; 5_000]));
        assert!(mem.total_bytes <= 1_000 || mem.entries.is_empty());
    }

    #[test]
    fn mem_cache_updating_an_existing_key_accounts_for_the_size_delta() {
        let mut mem = MemCache::new(150);
        mem.put("a".to_string(), Arc::new(vec![0u8; 100]));
        // Replacing "a" with a much smaller payload must free up room
        // rather than double-counting the old size.
        mem.put("a".to_string(), Arc::new(vec![0u8; 10]));
        mem.put("b".to_string(), Arc::new(vec![0u8; 100]));
        assert!(mem.get("a").is_some());
        assert!(mem.get("b").is_some());
        assert_eq!(mem.total_bytes, 110);
    }

    // ---- bounded image body reads -----------------------------

    /// One-shot local HTTP server that answers any request with `body`
    /// (200 OK, `Content-Length` set). Local-only (127.0.0.1, ephemeral
    /// port); same shape as `jellyfin-api`'s own raw-TCP test servers.
    async fn body_server(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut discard = [0u8; 1024];
                let _ = stream.read(&mut discard).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                // A client that hit the cap drops the connection mid-body,
                // so this write legitimately fails -- ignore it.
                let _ = stream.write_all(&body).await;
                let _ = stream.shutdown().await;
            }
        });
        format!("http://{addr}/")
    }

    /// A body at or under the cap still round-trips byte for byte
    /// -- the cap must not truncate or corrupt normal responses.
    #[tokio::test]
    async fn read_capped_returns_a_body_that_fits() {
        let url = body_server(vec![7u8; 4096]).await;
        let resp = reqwest::get(&url).await.expect("request");
        let body = read_capped(resp, 8192).await.expect("under the cap");
        assert_eq!(body, vec![7u8; 4096]);
    }

    /// Before this, `Response::bytes()` buffered whatever the
    /// server sent -- a hostile server could force a multi-gigabyte
    /// allocation with a single image request. Now the read stops (and the
    /// connection drops) as soon as the running total would exceed the
    /// ceiling, and the caller gets a clean error instead of a giant Vec.
    #[tokio::test]
    async fn read_capped_refuses_a_body_over_the_cap() {
        let url = body_server(vec![7u8; 64 * 1024]).await;
        let resp = reqwest::get(&url).await.expect("request");
        let err = read_capped(resp, 1024).await.expect_err("over the cap");
        assert!(err.contains("exceeded"), "unexpected error: {err}");
    }
}
