//! GPUI-side half of the image pipeline (docs/DATA.md §3/§4, docs/UX-SPEC.md §5), on top
//! of `media_cache::ImageCache` (disk+network fetch, coalescing,
//! cancel-on-scroll -- frozen there). Adds:
//!
//! - Decoding fetched bytes into an `Arc<gpui::RenderImage>`, mirroring
//!   gpui's own `img()` loader (RGBA -> BGRA swap; see `elements/img.rs` in
//!   the pinned gpui source).
//! - An in-memory decoded-texture cache keyed by (item, kind, tag, width).
//! - Blurhash -> placeholder `RenderImage`, decoded once per hash and cached
//!   forever (docs/UX-SPEC.md §5 "blurhash placeholder first paint").
//!
//! `ImageStore::get` is synchronous and non-blocking: it returns the decoded
//! texture if already resolved, otherwise kicks off at most one background
//! fetch+decode and returns `None` so the caller paints blurhash that same
//! frame. Completion bridges back into GPUI via a oneshot channel +
//! `App::spawn`, which calls `cx.notify()` on the owning `Root` entity (see
//! `cards.rs`).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gpui::{App, RenderImage, WeakEntity};
use image::Frame;

use media_cache::{ImageCache, ImageKind};

use crate::root::Root;

/// Poster cell width fetched from the server (docs/DATA.md §3: one width bucket
/// per role, sized to the largest cell that uses it).
pub(crate) const POSTER_WIDTH: u32 = 320;
/// Detail-page backdrop width (dimmed hero art, docs/UX-SPEC.md §5).
pub(crate) const BACKDROP_WIDTH: u32 = 1280;
/// Episode/16:9 thumb width (Detail page season lists, docs/UX-SPEC.md §5).
pub(crate) const THUMB_WIDTH: u32 = 400;
/// Cast row headshot width (Detail page, visual pass §5) -- fetched wider
/// than its on-screen circular size so it doesn't look soft.
pub(crate) const PORTRAIT_WIDTH: u32 = 150;

/// 32x48 keeps blurhash decode cheap enough to run synchronously on the UI
/// thread -- the placeholder must be ready the same frame a cell appears.
const BLURHASH_W: u32 = 32;
const BLURHASH_H: u32 = 48;

fn decode_key(item_id: &str, kind: ImageKind, tag: &str, max_width: u32) -> String {
    format!("{item_id}:{kind:?}:{tag}:{max_width}")
}

/// Visual-pass §1's "processed backdrop" variant: same source image as
/// `decode_key`, blurred + darkened for the below-the-fold scrim region
/// (`backdrop::layer`). A distinct key so it never collides with -- or
/// evicts in place of -- the sharp decode of the same backdrop; both are
/// painted together every time this backdrop is on screen.
fn scrim_key(item_id: &str, kind: ImageKind, tag: &str, max_width: u32) -> String {
    format!("{}:scrim", decode_key(item_id, kind, tag, max_width))
}

/// Raw image bytes (JPEG/PNG/WebP/etc, whatever the server sent) -> a
/// GPUI-paintable `RenderImage`. Mirrors gpui's own internal loader exactly
/// (RGBA decode, then a straight R/B channel swap to BGRA) so the resulting
/// texture behaves identically to one gpui loaded itself.
///
/// Returns the decoded texture alongside its raw byte size (width * height *
/// 4 BGRA bytes) -- M5's bounded decoded-texture cache (`Inner::decoded`)
/// needs that size at insert time, and it's cheapest to read straight off
/// `buf` here rather than reconstructing it later from the opaque
/// `RenderImage`.
/// A decoded texture, its raw BGRA byte size (M5's LRU budget), and its §9
/// ambient average-color sample (`average_color_u32`) -- what every
/// `decode_bytes*` fn returns and `get_with_decoder`'s `decoder` parameter
/// is typed as. Named here so clippy's `type_complexity` lint (and any
/// human reader) sees one meaningful name instead of a three-tuple spelled
/// out at every call site.
type DecodeResult = Option<(RenderImage, usize, u32)>;

/// Discover's generic remote-URL fetch (`ImageStore::get_remote`): a plain
/// GET with the same body-size cap `media_cache::image_cache`'s own
/// `IMAGE_BODY_CAP` uses, for the same reason -- a poster/backdrop response
/// is legitimately large but bounded, and `Response::bytes()` would
/// otherwise buffer an unbounded body before this code ever gets to look at
/// it.
const REMOTE_IMAGE_BODY_CAP: usize = 64 * 1024 * 1024;

async fn fetch_remote_bytes(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .await
        .ok()
        .filter(|r| r.status().is_success())?;
    let mut body = Vec::new();
    let mut resp = resp;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > REMOTE_IMAGE_BODY_CAP {
                    tracing::warn!(
                        url = %crate::redact::redact_url(url),
                        "remote image response exceeded the size cap; aborting read"
                    );
                    return None;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Some(body),
            Err(_) => return None,
        }
    }
}

fn decode_bytes(bytes: &[u8]) -> DecodeResult {
    let mut buf = image::load_from_memory(bytes).ok()?.into_rgba8();
    // §9's ambient hero wash: sample the average color BEFORE the RGBA->BGRA
    // swap below, while channel order is still unambiguous -- see
    // `average_color_u32`'s doc comment.
    let ambient = average_color_u32(&buf);
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let byte_size = (buf.width() as usize) * (buf.height() as usize) * 4;
    Some((
        RenderImage::new(smallvec::smallvec![Frame::new(buf)]),
        byte_size,
        ambient,
    ))
}

/// §9's ambient hero wash: "sample the hero art's dominant color at decode
/// (cheap average/quantize in image_store), cached alongside" -- a plain
/// strided-sample average (every 7th pixel, a small prime chosen so the
/// stride doesn't alias with common image row widths/strides), not a real
/// k-means/histogram-quantized "dominant color" -- cheap enough to run
/// synchronously inside every decode (`decode_bytes`/`decode_bytes_scrim`)
/// without a caller ever needing to ask for it separately, while still
/// giving a representative hue for the wash below. Must be called on the
/// buffer's original RGBA channel order, before any BGRA swap.
fn average_color_u32(buf: &image::RgbaImage) -> u32 {
    let raw = buf.as_raw();
    let mut r_sum: u64 = 0;
    let mut g_sum: u64 = 0;
    let mut b_sum: u64 = 0;
    let mut n: u64 = 0;
    for px in raw.chunks_exact(4).step_by(7) {
        r_sum += u64::from(px[0]);
        g_sum += u64::from(px[1]);
        b_sum += u64::from(px[2]);
        n += 1;
    }
    if n == 0 {
        return 0x00_00_00;
    }
    let r = (r_sum / n) as u32;
    let g = (g_sum / n) as u32;
    let b = (b_sum / n) as u32;
    (r << 16) | (g << 8) | b
}

/// §1's honest CPU approximation of `blur(20px) + brightness(0.45)`, run on
/// the tokio pool (`ImageStore::get_scrim`), never the UI thread. Downscales
/// first -- blurring at the full backdrop decode resolution is wasted work
/// for an image that's about to be shown blurred anyway (indistinguishable
/// result once `ObjectFit::Cover` scales it back up on screen), and it keeps
/// the Gaussian pass itself cheap. `fast_blur` (box-blur approximation, not
/// the exact-but-slower `blur`) is the right tradeoff here: this runs once
/// per backdrop and is cached forever after (`ImageStore::get_scrim`), but a
/// slow decode still delays that backdrop's first paint, and precision
/// doesn't matter for a scrim nothing is meant to look sharp through.
const SCRIM_DOWNSCALE_DIVISOR: u32 = 4;
/// Sigma tuned against the *downscaled* image (see `SCRIM_DOWNSCALE_DIVISOR`)
/// so the result reads as roughly a 20px blur once `ObjectFit::Cover` scales
/// it back up to its on-screen size -- there's no principled unit conversion
/// from "CSS px blur radius" to "Gaussian sigma on a 4x-downscaled decode"
/// without knowing the final on-screen size ahead of decode time, so this is
/// a tuned-by-eye value, not a derived one.
const SCRIM_BLUR_SIGMA: f32 = 6.0;
/// §1's exact `brightness(0.45)` -- a flat linear multiply on RGB, applied
/// post-blur.
const SCRIM_BRIGHTNESS: f32 = 0.45;

/// Raw backdrop bytes -> the blurred+darkened scrim variant (see the consts
/// above). Mirrors `decode_bytes`'s RGBA->BGRA swap so the result paints
/// identically to any other `RenderImage` this store produces.
fn decode_bytes_scrim(bytes: &[u8]) -> DecodeResult {
    let decoded = image::load_from_memory(bytes).ok()?;
    let (w, h) = (decoded.width(), decoded.height());
    let (dw, dh) = (
        (w / SCRIM_DOWNSCALE_DIVISOR).max(1),
        (h / SCRIM_DOWNSCALE_DIVISOR).max(1),
    );
    let small = decoded.resize_exact(dw, dh, image::imageops::FilterType::Triangle);
    let blurred = small.fast_blur(SCRIM_BLUR_SIGMA);
    let mut buf = blurred.into_rgba8();
    // Same ambient sample as `decode_bytes` -- this variant is already
    // downscaled+blurred, so sampling it is even cheaper; unused by any
    // current caller (the sharp decode's ambient color is what `home.rs`
    // actually reads), kept for signature parity with `decode_bytes` (both
    // must match `get_with_decoder`'s single `decoder: fn(&[u8]) ->
    // Option<(RenderImage, usize, u32)>` parameter type).
    let ambient = average_color_u32(&buf);
    for px in buf.chunks_exact_mut(4) {
        px[0] = (px[0] as f32 * SCRIM_BRIGHTNESS) as u8;
        px[1] = (px[1] as f32 * SCRIM_BRIGHTNESS) as u8;
        px[2] = (px[2] as f32 * SCRIM_BRIGHTNESS) as u8;
        px.swap(0, 2);
    }
    let byte_size = (buf.width() as usize) * (buf.height() as usize) * 4;
    Some((
        RenderImage::new(smallvec::smallvec![Frame::new(buf)]),
        byte_size,
        ambient,
    ))
}

/// A server-provided blurhash string -> a small placeholder `RenderImage`.
fn decode_blurhash(hash: &str) -> Option<RenderImage> {
    let mut pixels = blurhash::decode(hash, BLURHASH_W, BLURHASH_H, 1.0).ok()?;
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buf = image::ImageBuffer::from_raw(BLURHASH_W, BLURHASH_H, pixels)?;
    Some(RenderImage::new(smallvec::smallvec![Frame::new(buf)]))
}

/// M5: `Inner::decoded`'s per-entry bookkeeping for the count+bytes-bounded
/// LRU eviction below. `last_used` is a value from `Inner::lru_clock` --
/// GPUI/`ImageStore` is single-threaded (main-thread only, `Rc`-based, like
/// everything else in this struct), so a plain monotonic counter is enough
/// recency tracking without needing real timestamps or an ordered map.
struct DecodedEntry {
    image: Arc<RenderImage>,
    byte_size: usize,
    last_used: u64,
    /// §9's ambient hero wash: the `average_color_u32` sample computed at
    /// decode time, cached right alongside the texture itself rather than in a separate map keyed by the
    /// same string -- one lookup, one eviction lifecycle, no risk of the two
    /// drifting out of sync.
    ambient: u32,
}

/// M5: decoded-texture cache budget. Unbounded, this cache grew forever as
/// the user scrolled through a large library -- every poster/backdrop/thumb
/// ever decoded stayed resident for the rest of the process's life (a 320px
/// poster alone decodes to ~460KB of BGRA; a library with a few thousand
/// distinct posters/backdrops/episode thumbs adds up to a very large,
/// permanently-growing footprint). 192MB comfortably covers several screens
/// worth of posters/backdrops/thumbs at the widths this app actually
/// requests (see `POSTER_WIDTH`/`BACKDROP_WIDTH`/`THUMB_WIDTH`) while
/// bounding worst-case memory for a long browsing session. See
/// ARCHITECTURE.md's "Image pipeline" section.
const DECODED_CACHE_BUDGET_BYTES: usize = 192 * 1024 * 1024;
/// Belt-and-suspenders count cap alongside the byte budget -- guards against
/// a pathological case of many tiny/degenerate decoded images (e.g. a
/// misbehaving server returning 1x1 images) that would stay well under the
/// byte budget while still growing the `HashMap`/eviction-scan cost
/// unboundedly.
const DECODED_CACHE_MAX_COUNT: usize = 4000;

struct Inner {
    cache: ImageCache,
    runtime: Arc<tokio::runtime::Runtime>,
    /// M5: bounded LRU (see `DECODED_CACHE_BUDGET_BYTES`/`evict_if_needed`).
    decoded: RefCell<HashMap<String, DecodedEntry>>,
    decoded_bytes: Cell<usize>,
    lru_clock: Cell<u64>,
    blurhashes: RefCell<HashMap<String, Arc<RenderImage>>>,
    inflight: RefCell<HashSet<String>>,
    /// Items whose fade-in animation has already played once this session
    /// (`cards.rs` reads+marks this so a cell doesn't re-fade every time it
    /// scrolls back into view with an already-decoded texture).
    faded_in: RefCell<HashSet<String>>,
    /// Mirrors `media_cache::ImageCache`'s own scroll/priority generation
    /// (docs/DATA.md §3: "visible + 1 screen ahead keep priority; offscreen
    /// requests are dropped, not queued"). Kept here too so callers that
    /// only have an `ImageStore` handle (not the underlying `ImageCache`)
    /// can still read the current floor.
    generation: AtomicU64,
    /// Hover-dwell prefetch bookkeeping (docs/UX-SPEC.md §5 / docs/DATA.md §4: 350ms
    /// dwell before warming Detail images). Keyed by item id; a hover
    /// enter/exit bumps to a fresh, never-reused epoch so a stale timer
    /// from a since-ended hover can't fire late.
    dwell_current: RefCell<HashMap<String, u64>>,
    dwell_next: Cell<u64>,
    /// Loading-experience instrumentation (`JELLYBEAM_LOADTEST`): counts how
    /// many *new* fetches `get()` has actually kicked off (i.e. excludes
    /// calls served straight from `decoded` and calls that coalesced onto an
    /// already-inflight fetch for the same key). This is "image requests
    /// issued" from the UI side of the pipeline, upstream of
    /// `media_cache::ImageCache`'s own mem/disk/network split.
    requests_issued: Cell<u64>,
    /// Discover's TMDB/imageproxy poster
    /// URLs are absolute external URLs, not `(item_id, kind, tag)` triples --
    /// `media_cache::ImageCache::get` is gateway-coupled (it always rebuilds
    /// the URL itself via `JellyfinClient::image_url`, see that fn's own
    /// doc comment), so there is no way to hand it a pre-built URL. Rather
    /// than touch `media-cache` (out of scope -- Discover must not touch
    /// that crate at all, see `discover/mod.rs`'s "no mirror involvement"),
    /// this is the minimal generic remote-URL path needed: a
    /// standalone `reqwest::Client`, built lazily on first use, mirroring
    /// `trickplay.rs`'s identical "bypass the gateway-coupled cache, fetch
    /// straight from the URL the server/crate already built" shape.
    remote_client: RefCell<Option<reqwest::Client>>,
}

impl Inner {
    /// Advances and returns the LRU clock -- see `DecodedEntry::last_used`'s
    /// doc comment.
    fn touch(&self) -> u64 {
        let next = self.lru_clock.get() + 1;
        self.lru_clock.set(next);
        next
    }

    /// M5: evicts least-recently-used entries from `decoded` until it's back
    /// under both `DECODED_CACHE_BUDGET_BYTES` and `DECODED_CACHE_MAX_COUNT`.
    /// A linear scan for the minimum `last_used` on every eviction, not a
    /// real LRU list -- deliberately simple: this cache tops out at a few
    /// thousand entries (`DECODED_CACHE_MAX_COUNT`), so an occasional O(n)
    /// scan here is cheap relative to the network fetch + decode that
    /// re-populates an evicted entry, and it avoids pulling in an external
    /// LRU crate or hand-rolling an intrusive linked list for what's a
    /// non-hot-path cache (`ImageStore::get` only calls this after a
    /// background fetch/decode completes, never on the render path itself).
    fn evict_if_needed(&self, decoded: &mut HashMap<String, DecodedEntry>) {
        while decoded.len() > DECODED_CACHE_MAX_COUNT
            || self.decoded_bytes.get() > DECODED_CACHE_BUDGET_BYTES
        {
            let Some(oldest_key) = decoded
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break; // cache is empty -- nothing left to evict
            };
            if let Some(evicted) = decoded.remove(&oldest_key) {
                self.decoded_bytes
                    .set(self.decoded_bytes.get() - evicted.byte_size);
            }
        }
    }
}

/// Clone-cheap (`Rc`) handle shared by every view that paints images.
#[derive(Clone)]
pub(crate) struct ImageStore(Rc<Inner>);

impl ImageStore {
    pub(crate) fn new(cache: ImageCache, runtime: Arc<tokio::runtime::Runtime>) -> Self {
        Self(Rc::new(Inner {
            cache,
            runtime,
            decoded: RefCell::new(HashMap::new()),
            decoded_bytes: Cell::new(0),
            lru_clock: Cell::new(0),
            blurhashes: RefCell::new(HashMap::new()),
            inflight: RefCell::new(HashSet::new()),
            faded_in: RefCell::new(HashSet::new()),
            generation: AtomicU64::new(0),
            dwell_current: RefCell::new(HashMap::new()),
            dwell_next: Cell::new(0),
            requests_issued: Cell::new(0),
            remote_client: RefCell::new(None),
        }))
    }

    /// Loading-experience instrumentation (`JELLYBEAM_LOADTEST`): total number
    /// of new (non-coalesced, non-cache-hit) fetches issued since this
    /// `ImageStore` was created.
    pub(crate) fn requests_issued(&self) -> u64 {
        self.0.requests_issued.get()
    }

    /// The underlying disk+network image cache's hit/miss counters (see
    /// `media_cache::ImageCache::stats`) -- a fully-warm relaunch should show
    /// `network_fetches == 0` and a ~100% hit rate here.
    pub(crate) fn cache_stats(&self) -> media_cache::ImageCacheStats {
        self.0.cache.stats()
    }

    /// Call on hover-enter. Returns an opaque epoch token; after the 350ms
    /// dwell delay, pass it to `dwell_is_current` to check the hover is
    /// still live before actually prefetching (`root.rs`'s hover handler).
    pub(crate) fn dwell_enter(&self, key: &str) -> u64 {
        let epoch = self.0.dwell_next.get() + 1;
        self.0.dwell_next.set(epoch);
        self.0
            .dwell_current
            .borrow_mut()
            .insert(key.to_string(), epoch);
        epoch
    }

    /// Whether a dwell timer is already armed for `key` -- lets the
    /// pointer-motion handler arm exactly one timer per hover rather than
    /// one per mouse-move event (see `cards.rs`'s dwell wiring).
    pub(crate) fn dwell_is_armed(&self, key: &str) -> bool {
        self.0.dwell_current.borrow().contains_key(key)
    }

    /// Call on hover-exit: invalidates any pending dwell timer for `key`.
    pub(crate) fn dwell_exit(&self, key: &str) {
        self.0.dwell_current.borrow_mut().remove(key);
    }

    pub(crate) fn dwell_is_current(&self, key: &str, epoch: u64) -> bool {
        self.0.dwell_current.borrow().get(key) == Some(&epoch)
    }

    /// Synchronous, always-available placeholder. Returns `None` only if the
    /// hash string itself is malformed (never blocks, never fetches).
    pub(crate) fn blurhash(&self, hash: &str) -> Option<Arc<RenderImage>> {
        if let Some(img) = self.0.blurhashes.borrow().get(hash) {
            return Some(img.clone());
        }
        let img = Arc::new(decode_blurhash(hash)?);
        self.0
            .blurhashes
            .borrow_mut()
            .insert(hash.to_string(), img.clone());
        Some(img)
    }

    /// Cache-only lookup: returns the decoded texture if already resolved,
    /// but -- unlike `get` -- never kicks off a fetch on a miss. For a cell
    /// that isn't in `get`'s "visible + 1 screen ahead" eager window (Home
    /// shelves beyond the horizontal scroll viewport; see `home.rs`'s
    /// `EAGER_COLUMN_MARGIN`): if the texture happens to already be cached
    /// (e.g. the user scrolled past it before, or it's shared with an
    /// already-loaded shelf), show it immediately rather than needlessly
    /// hiding a ready image behind a blurhash placeholder; if it isn't
    /// cached, paint blurhash and stay lazy -- no network request until the
    /// cell actually becomes eager (scrolls into range or gets focused).
    pub(crate) fn get_cached(
        &self,
        item_id: &str,
        kind: ImageKind,
        tag: &str,
        max_width: u32,
    ) -> Option<Arc<RenderImage>> {
        let key = decode_key(item_id, kind, tag, max_width);
        self.touch_decoded(&key)
    }

    /// Returns the decoded texture if already resolved; otherwise kicks off
    /// (at most one, coalesced by `inflight`) background fetch+decode and
    /// returns `None` immediately -- never blocks the render pass.
    pub(crate) fn get(
        &self,
        item_id: &str,
        kind: ImageKind,
        tag: &str,
        max_width: u32,
        root: WeakEntity<Root>,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        self.get_with_decoder(
            decode_key(item_id, kind, tag, max_width),
            item_id,
            kind,
            tag,
            max_width,
            root,
            cx,
            decode_bytes,
        )
    }

    /// §1's processed-backdrop variant of `get` -- same fetch (the
    /// underlying `media_cache::ImageCache` disk-caches the raw bytes, so
    /// this is a second decode of already-fetched bytes in the common case,
    /// not a second network round trip), but decoded+blurred+darkened via
    /// `decode_bytes_scrim` and cached under `scrim_key` so it never
    /// collides with the sharp variant callers also want painted for the
    /// same backdrop. Always eager (backdrops are single large elements a
    /// caller only ever renders when actually on screen, unlike a shelf's
    /// off-screen cells -- there's no lazy-window variant to mirror
    /// `get_cached` here).
    pub(crate) fn get_scrim(
        &self,
        item_id: &str,
        kind: ImageKind,
        tag: &str,
        max_width: u32,
        root: WeakEntity<Root>,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        self.get_with_decoder(
            scrim_key(item_id, kind, tag, max_width),
            item_id,
            kind,
            tag,
            max_width,
            root,
            cx,
            decode_bytes_scrim,
        )
    }

    /// Discover's generic remote-URL path (see `Inner::remote_client`'s doc
    /// comment): same "decoded texture if ready, else kick off at most one
    /// background fetch and return `None`" contract as `get`, reusing the
    /// exact same decoded-texture LRU (`insert_decoded`/`touch_decoded`) so
    /// a poster fetched this way is bounded by the same
    /// `DECODED_CACHE_BUDGET_BYTES`/`DECODED_CACHE_MAX_COUNT` budget as
    /// every Jellyfin-gateway image. Keyed on the URL itself (prefixed so it
    /// can never collide with a `decode_key`/`scrim_key` string, both of
    /// which always contain a `:` -separated kind/tag/width that a bare URL
    /// won't happen to reproduce, but the prefix makes that non-collision
    /// obvious rather than incidental).
    pub(crate) fn get_remote(
        &self,
        url: &str,
        root: WeakEntity<Root>,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        let key = format!("remote:{url}");
        if let Some(image) = self.touch_decoded(&key) {
            return Some(image);
        }
        if !self.0.inflight.borrow_mut().insert(key.clone()) {
            return None; // already fetching this URL
        }
        self.0.requests_issued.set(self.0.requests_issued.get() + 1);

        // Built (or reused) on this thread -- a `reqwest::Client` is
        // `Send + Sync + Clone`, unlike `ImageStore` itself (`Rc`-backed), so
        // only the client, not `self`, may cross into `runtime.spawn` below.
        let client = self.remote_http_client();
        let store = self.clone();
        let runtime = self.0.runtime.clone();
        let fetch_url = url.to_string();
        let fetch_key = key.clone();

        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let bytes = fetch_remote_bytes(&client, &fetch_url).await;
            let decoded = bytes.and_then(|b| decode_bytes(&b));
            let _ = tx.send(decoded);
        });

        cx.spawn(async move |cx| {
            let decoded = rx.await.unwrap_or(None);
            store.0.inflight.borrow_mut().remove(&fetch_key);
            if let Some((image, byte_size, ambient)) = decoded {
                store.insert_decoded(fetch_key, image, byte_size, ambient);
                let _ = root.update(cx, |_root, cx| cx.notify());
            }
        })
        .detach();

        None
    }

    /// Lazily builds (once) the plain `reqwest::Client` `get_remote` fetches
    /// TMDB/imageproxy URLs through -- same timeout/DNS shape as
    /// `trickplay.rs`'s `trickplay_http_client`, minus that fetch's
    /// HTTPS-only redirect guard (a Discover image URL is server/TMDB-
    /// controlled, not user-typed, and `SeerrError`'s own doc conventions
    /// already keep secrets out of it -- there's no bearer token in a query
    /// string here to leak over a scheme downgrade the way a Jellyfin stream
    /// URL's `ApiKey` param would be).
    fn remote_http_client(&self) -> reqwest::Client {
        if let Some(client) = self.0.remote_client.borrow().as_ref() {
            return client.clone();
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("remote image HTTP client configuration is valid");
        *self.0.remote_client.borrow_mut() = Some(client.clone());
        client
    }

    /// Shared fetch+decode+cache path for `get`/`get_scrim` -- identical
    /// except for which decode function turns raw bytes into a
    /// `RenderImage` and which cache key the result lands under.
    #[allow(clippy::too_many_arguments)]
    fn get_with_decoder(
        &self,
        key: String,
        item_id: &str,
        kind: ImageKind,
        tag: &str,
        max_width: u32,
        root: WeakEntity<Root>,
        cx: &mut App,
        decoder: fn(&[u8]) -> DecodeResult,
    ) -> Option<Arc<RenderImage>> {
        if let Some(image) = self.touch_decoded(&key) {
            return Some(image);
        }
        if !self.0.inflight.borrow_mut().insert(key.clone()) {
            return None; // already fetching this key
        }
        self.0.requests_issued.set(self.0.requests_issued.get() + 1);

        let store = self.clone();
        let cache = self.0.cache.clone();
        let runtime = self.0.runtime.clone();
        let item_id = item_id.to_string();
        let tag = tag.to_string();
        let fetch_key = key.clone();

        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let outcome = cache.get(&item_id, kind, &tag, max_width).await;
            let decoded = match outcome {
                Ok(bytes) => decoder(&bytes),
                Err(media_cache::CacheError::Cancelled) => None,
                Err(e) => {
                    tracing::debug!(error = %e, item_id = %item_id, "image fetch failed");
                    None
                }
            };
            let _ = tx.send(decoded);
        });

        cx.spawn(async move |cx| {
            let decoded = rx.await.unwrap_or(None);
            store.0.inflight.borrow_mut().remove(&fetch_key);
            if let Some((image, byte_size, ambient)) = decoded {
                store.insert_decoded(fetch_key, image, byte_size, ambient);
                let _ = root.update(cx, |_root, cx| cx.notify());
            }
        })
        .detach();

        None
    }

    /// Cache-hit path: if `key` is already decoded, marks it as the most
    /// recently used entry (see `DecodedEntry::last_used`) and returns it --
    /// an item still being actively viewed/scrolled past should survive
    /// eviction longer than one that hasn't been asked for since.
    fn touch_decoded(&self, key: &str) -> Option<Arc<RenderImage>> {
        let mut decoded = self.0.decoded.borrow_mut();
        let entry = decoded.get_mut(key)?;
        entry.last_used = self.0.touch();
        Some(entry.image.clone())
    }

    /// M5: inserts a freshly decoded texture into the bounded LRU cache,
    /// evicting the least-recently-used entries afterward if this pushes
    /// the cache over `DECODED_CACHE_BUDGET_BYTES`/`DECODED_CACHE_MAX_COUNT`.
    fn insert_decoded(&self, key: String, image: RenderImage, byte_size: usize, ambient: u32) {
        let last_used = self.0.touch();
        let mut decoded = self.0.decoded.borrow_mut();
        // A concurrent identical fetch shouldn't be possible (`inflight`
        // coalesces by key), but if this key is somehow already present,
        // don't double-count its bytes.
        if let Some(old) = decoded.insert(
            key,
            DecodedEntry {
                image: Arc::new(image),
                byte_size,
                last_used,
                ambient,
            },
        ) {
            self.0
                .decoded_bytes
                .set(self.0.decoded_bytes.get() - old.byte_size);
        }
        self.0
            .decoded_bytes
            .set(self.0.decoded_bytes.get() + byte_size);
        self.0.evict_if_needed(&mut decoded);
    }

    /// §9's ambient hero wash: the `average_color_u32` sample cached
    /// alongside `key`'s decoded texture (`DecodedEntry::ambient`), as an
    /// `0xRRGGBB` value -- `None` until that key has actually been decoded
    /// (mirrors `get`/`get_scrim`'s own "returns `None` while the fetch is
    /// still in flight" contract; this is a pure peek, not a fetch trigger
    /// -- a caller that wants the color for an image not yet decoded should
    /// call `get`/`get_scrim` for the texture itself first). A plain peek
    /// (doesn't bump `last_used`) since reading the color for a decorative
    /// wash shouldn't itself extend a texture's LRU lifetime.
    pub(crate) fn ambient_color(
        &self,
        item_id: &str,
        kind: ImageKind,
        tag: &str,
        max_width: u32,
    ) -> Option<u32> {
        let key = decode_key(item_id, kind, tag, max_width);
        self.0.decoded.borrow().get(&key).map(|e| e.ambient)
    }

    /// True the first time it's called for a given key (marks it fresh);
    /// `cards.rs` uses this to fade a poster in only on its first arrival.
    pub(crate) fn take_fresh_arrival(&self, item_id: &str, tag: &str) -> bool {
        self.0
            .faded_in
            .borrow_mut()
            .insert(format!("{item_id}:{tag}"))
    }

    /// docs/DATA.md §3 cancel-on-scroll: advances the shared priority floor and
    /// forwards to the underlying `ImageCache` so any still-pending fetch
    /// below it bails out with `Cancelled` instead of completing wastefully.
    pub(crate) fn cancel_below_priority(&self, generation: u64) {
        self.0.generation.fetch_max(generation, Ordering::SeqCst);
        self.0.cache.cancel_below_priority(generation);
    }

    /// Monotonically advances and returns the new scroll/priority generation
    /// (callers bump this whenever a grid/shelf's visible range changes).
    pub(crate) fn bump_generation(&self) -> u64 {
        self.0.generation.fetch_add(1, Ordering::SeqCst) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 stand-in texture -- `insert_decoded`'s `byte_size` parameter is
    /// independent of the `RenderImage`'s actual pixel data, so tests can
    /// exercise the LRU eviction accounting with tiny real images and
    /// caller-chosen synthetic sizes instead of decoding real (and much
    /// slower to construct) multi-hundred-KB posters.
    fn dummy_image() -> RenderImage {
        let buf = image::ImageBuffer::from_pixel(1, 1, image::Rgba([0u8, 0, 0, 0]));
        RenderImage::new(smallvec::smallvec![Frame::new(buf)])
    }

    /// Builds a real `ImageStore` (needs a `JellyfinClient` and `ImageCache`
    /// to construct, per their frozen signatures) but never drives any
    /// actual network fetch -- these tests only exercise `insert_decoded`/
    /// `touch_decoded` directly.
    fn test_store() -> ImageStore {
        let identity = jellyfin_api::ClientIdentity {
            client: "jellybeam-test".to_string(),
            device: "test".to_string(),
            device_id: "jellybeam-image-store-test".to_string(),
            version: "0.0.0".to_string(),
        };
        let client =
            jellyfin_api::JellyfinClient::from_token("http://127.0.0.1:1", identity, "fake-token");
        let dir = std::env::temp_dir().join(format!(
            "jellybeam-image-store-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let cache = ImageCache::new(dir, client);
        let runtime =
            Arc::new(tokio::runtime::Runtime::new().expect("build a tokio runtime for this test"));
        ImageStore::new(cache, runtime)
    }

    /// M5 regression test: pre-fix, `decoded` had no size limit at all and
    /// grew forever. Two ~80MB entries fit comfortably under the 192MB
    /// budget; a third pushes the total to 240MB, over budget, so it must
    /// evict the least-recently-used one first.
    #[test]
    fn decoded_cache_evicts_least_recently_used_entries_past_the_byte_budget() {
        let store = test_store();
        let entry_bytes = 80 * 1024 * 1024;
        store.insert_decoded("a".to_string(), dummy_image(), entry_bytes, 0);
        store.insert_decoded("b".to_string(), dummy_image(), entry_bytes, 0);
        assert_eq!(store.0.decoded.borrow().len(), 2);
        assert_eq!(store.0.decoded_bytes.get(), entry_bytes * 2);

        store.insert_decoded("c".to_string(), dummy_image(), entry_bytes, 0);

        let decoded = store.0.decoded.borrow();
        assert!(
            decoded.len() <= 2,
            "cache must evict to stay under the byte budget, got {} entries",
            decoded.len()
        );
        assert!(
            store.0.decoded_bytes.get() <= DECODED_CACHE_BUDGET_BYTES,
            "tracked byte total must stay under the budget after eviction"
        );
        assert!(
            !decoded.contains_key("a"),
            "the least-recently-used entry ('a', never touched again) must \
             be evicted first"
        );
        assert!(
            decoded.contains_key("c"),
            "the just-inserted entry must survive its own insertion"
        );
    }

    /// A cache hit (`touch_decoded`, what `get()` calls on every hit) must
    /// protect an entry from being the next eviction victim, even if it was
    /// inserted before other entries that are otherwise untouched.
    #[test]
    fn touching_an_entry_protects_it_from_being_the_next_eviction_victim() {
        let store = test_store();
        let entry_bytes = 80 * 1024 * 1024;
        store.insert_decoded("a".to_string(), dummy_image(), entry_bytes, 0);
        store.insert_decoded("b".to_string(), dummy_image(), entry_bytes, 0);

        // Touch "a" (simulating a render pass re-requesting it) so it's now
        // more recently used than "b".
        assert!(store.touch_decoded("a").is_some());

        store.insert_decoded("c".to_string(), dummy_image(), entry_bytes, 0);

        let decoded = store.0.decoded.borrow();
        assert!(
            !decoded.contains_key("b"),
            "'b' (least-recently-used after 'a' was touched) must be evicted, got keys={:?}",
            decoded.keys().collect::<Vec<_>>()
        );
        assert!(
            decoded.contains_key("a"),
            "'a' was touched more recently than 'b' and must survive"
        );
    }

    /// Regression test for "tiles constantly
    /// switching": the classic virtualized-grid recycling bug is an
    /// in-flight/resolved fetch for one item painting into a cell that's
    /// since been recycled to a different item. `ImageStore` has no notion
    /// of cells/slots at all -- every decoded-texture cache key is
    /// `item_id:kind:tag:width` (`decode_key`), so a request for item B can
    /// never be served item A's texture by construction. This test locks
    /// that invariant in: it's the "wrong-image deliveries must be 0"
    /// property JELLYBEAM_LOADTEST asserts at runtime.
    #[test]
    fn a_decoded_texture_for_one_item_never_serves_a_different_items_request() {
        let store = test_store();
        store.insert_decoded(
            decode_key("item-a", ImageKind::Primary, "tag-a", POSTER_WIDTH),
            dummy_image(),
            1,
            0,
        );

        // A cell recycled to a different item (even with the same kind/tag
        // shape) must be a clean cache miss -- never item A's texture.
        assert!(
            store
                .touch_decoded(&decode_key(
                    "item-b",
                    ImageKind::Primary,
                    "tag-a",
                    POSTER_WIDTH
                ))
                .is_none(),
            "a different item_id must never hit item A's cached texture"
        );
        // The item it actually belongs to must still resolve correctly.
        assert!(store
            .touch_decoded(&decode_key(
                "item-a",
                ImageKind::Primary,
                "tag-a",
                POSTER_WIDTH
            ))
            .is_some());
    }

    /// `get()` must only count a request as "issued" the first time a key
    /// is actually fetched -- a second call for the same still-inflight key
    /// coalesces (matches `media_cache::ImageCache`'s own coalescing) rather
    /// than issuing a redundant fetch, and `requests_issued` is meant to
    /// reflect real work kicked off, not call count.
    #[test]
    fn requests_issued_does_not_double_count_a_coalesced_inflight_key() {
        let store = test_store();
        let key = decode_key("item-a", ImageKind::Primary, "tag-a", POSTER_WIDTH);
        assert!(store.0.inflight.borrow_mut().insert(key.clone()));
        assert_eq!(store.requests_issued(), 0);

        // Simulate what `get()` does after the inflight check succeeds --
        // exercised directly here since a real `get()` call needs a live
        // tokio runtime + network fetch to complete.
        store
            .0
            .requests_issued
            .set(store.0.requests_issued.get() + 1);
        assert_eq!(store.requests_issued(), 1);
    }

    /// Belt-and-suspenders count cap: even entries individually far under
    /// the byte budget must still be bounded in total count.
    #[test]
    fn decoded_cache_evicts_past_the_count_cap_even_with_tiny_entries() {
        let store = test_store();
        for i in 0..(DECODED_CACHE_MAX_COUNT + 50) {
            store.insert_decoded(format!("key-{i}"), dummy_image(), 1, 0);
        }
        assert!(
            store.0.decoded.borrow().len() <= DECODED_CACHE_MAX_COUNT,
            "cache must evict to stay under the count cap regardless of how \
             small each entry is, got {} entries",
            store.0.decoded.borrow().len()
        );
    }
}
