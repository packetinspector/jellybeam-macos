//! Scrubber hover preview (docs/UX-SPEC.md §4: "trickplay preview tile above
//! cursor... prefetched at playback start... no live decode during drag").
//!
//! Not routed through `media_cache::ImageCache`: trickplay sheets are a
//! different shape from the poster/backdrop pipeline -- one sheet covers
//! many timestamps, so this caches whole decoded sheets (keyed by sheet
//! index), not per-timestamp crops.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, RenderImage, WeakEntity};
use image::{Frame, GenericImageView, ImageReader, Limits};

use jellyfin_api::models::BaseItemDto;
use jellyfin_api::JellyfinClient;

use crate::root::Root;

/// One playback session's trickplay geometry, resolved once from
/// `BaseItemDto::trickplay` at playback start (`playback.rs`) -- picks the
/// manifest width closest to `PREFERRED_WIDTH` among whatever the server
/// advertises for the active media source.
#[derive(Debug, Clone)]
pub(crate) struct TrickplayMeta {
    pub item_id: String,
    pub media_source_id: Option<String>,
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,  // thumbnails per row
    pub tile_height: u32, // thumbnails per column
    pub interval_ms: u32,
    pub thumbnail_count: u32,
}

const PREFERRED_WIDTH: u32 = 320;
const MAX_TILE_DIMENSION: u32 = 4_096;
const MAX_TILES_PER_AXIS: u32 = 128;
const MAX_TILES_PER_SHEET: u32 = MAX_TILES_PER_AXIS * MAX_TILES_PER_AXIS;

/// Picks the best manifest entry for `media_source_id` (falling back to any
/// entry if the source id isn't a key -- mirrors servers that only ever key
/// by a single/default source), preferring the width closest to
/// `PREFERRED_WIDTH`.
pub(crate) fn resolve_trickplay(
    dto: &BaseItemDto,
    item_id: &str,
    media_source_id: &str,
) -> Option<TrickplayMeta> {
    let by_width = dto
        .trickplay
        .get(media_source_id)
        .or_else(|| dto.trickplay.values().next())?;
    let (_, info) = by_width
        .iter()
        .filter(|(_, info)| {
            let Some(width) = info.width else {
                return false;
            };
            let Some(height) = info.height else {
                return false;
            };
            let tile_width = info.tile_width.unwrap_or(1);
            let tile_height = info.tile_height.unwrap_or(1);
            width > 0
                && height > 0
                && width <= MAX_TILE_DIMENSION as i32
                && height <= MAX_TILE_DIMENSION as i32
                && tile_width > 0
                && tile_width <= MAX_TILES_PER_AXIS as i32
                && tile_height > 0
                && tile_height <= MAX_TILES_PER_AXIS as i32
                && tile_width
                    .checked_mul(tile_height)
                    .is_some_and(|n| n <= MAX_TILES_PER_SHEET as i32)
        })
        .min_by_key(|(_, info)| {
            let w = info.width.unwrap_or(0);
            (w - PREFERRED_WIDTH as i32).unsigned_abs()
        })?;
    Some(TrickplayMeta {
        item_id: item_id.to_string(),
        media_source_id: Some(media_source_id.to_string()),
        // The predicate above proves all conversions and grid arithmetic
        // are safe; keep the values as unsigned for later image operations.
        width: info.width? as u32,
        height: info.height? as u32,
        tile_width: info.tile_width.unwrap_or(1) as u32,
        tile_height: info.tile_height.unwrap_or(1) as u32,
        interval_ms: info.interval.unwrap_or(10_000).max(1) as u32,
        thumbnail_count: info.thumbnail_count.unwrap_or(0).max(0) as u32,
    })
}

fn decode_bytes(bytes: &[u8]) -> Option<image::RgbaImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODED_SHEET_DIMENSION);
    limits.max_image_height = Some(MAX_DECODED_SHEET_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_SHEET_BYTES as u64);
    reader.limits(limits);
    Some(reader.decode().ok()?.into_rgba8())
}

fn to_render_image(img: image::RgbaImage) -> RenderImage {
    let mut buf = img;
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA -> BGRA, matches `image_store.rs::decode_bytes`.
    }
    RenderImage::new(smallvec::smallvec![Frame::new(buf)])
}

/// Maps a hover position (milliseconds into the item) to which sheet JPEG
/// covers it and where within that sheet's grid, per the trickplay tile
/// math: `local_index = ms / interval_ms` addresses one thumbnail overall;
/// `tiles_per_sheet = tile_width * tile_height` thumbnails live in each
/// sheet, row-major.
fn locate(meta: &TrickplayMeta, time_ms: u32) -> Option<(u32, u32, u32)> {
    let tiles_per_sheet = meta.tile_width.checked_mul(meta.tile_height)?;
    if meta.width == 0
        || meta.height == 0
        || meta.interval_ms == 0
        || tiles_per_sheet == 0
        || tiles_per_sheet > MAX_TILES_PER_SHEET
    {
        return None;
    }
    let max_index = meta.thumbnail_count.saturating_sub(1);
    let local_index = (time_ms / meta.interval_ms).min(max_index);
    let sheet_index = local_index / tiles_per_sheet;
    let pos = local_index % tiles_per_sheet;
    let row = pos / meta.tile_width;
    let col = pos % meta.tile_width;
    Some((sheet_index, row, col))
}

struct Inner {
    client: JellyfinClient,
    tile_client: reqwest::Client,
    runtime: Arc<tokio::runtime::Runtime>,
    /// Decoded whole sprite sheets, keyed by sheet index -- one HTTP fetch
    /// covers many crops (see this module's doc comment).
    sheets: HashMap<u32, Arc<image::RgbaImage>>,
    sheet_bytes: usize,
    inflight: HashSet<u32>,
    /// Cropped-and-BGRA-swapped single tiles ready for GPUI to paint,
    /// keyed by `(sheet, row, col)` -- avoids re-cropping/re-swapping every
    /// render while the pointer sits still over one scrubber position.
    crops: HashMap<(u32, u32, u32), Arc<RenderImage>>,
}

/// `Rc`-cloned into `MainState` like `ImageStore` (single-threaded GPUI
/// side; the actual fetch runs on the shared tokio runtime). One instance
/// per playback session -- `root.rs` replaces it whenever a new item starts
/// playing, so a stale item's sheets don't linger.
#[derive(Clone)]
pub(crate) struct TrickplayCache {
    inner: Arc<Mutex<Inner>>,
}

impl TrickplayCache {
    pub(crate) fn new(client: JellyfinClient, runtime: Arc<tokio::runtime::Runtime>) -> Self {
        TrickplayCache {
            inner: Arc::new(Mutex::new(Inner {
                client,
                tile_client: trickplay_http_client(),
                runtime,
                sheets: HashMap::new(),
                sheet_bytes: 0,
                inflight: HashSet::new(),
                crops: HashMap::new(),
            })),
        }
    }

    /// Read-only peek: returns the decoded tile if already cached, else
    /// `None` -- never kicks a fetch (no `cx`/owner needed). Used by
    /// `player_ui.rs`'s pure `render_trickplay_preview`; the actual
    /// fetch-kicking happens from `root_playback.rs::scrub_hover` (which does have a
    /// `Context<Root>`) via `tile_for_ms`.
    pub(crate) fn peek(&self, meta: &TrickplayMeta, time_ms: u32) -> Option<Arc<RenderImage>> {
        let (sheet, row, col) = locate(meta, time_ms)?;
        lock_ignore_poison(&self.inner)
            .crops
            .get(&(sheet, row, col))
            .cloned()
    }

    /// Synchronous, non-blocking: returns the decoded tile if already
    /// cached, else kicks off (at most one, de-duped by `inflight`) a
    /// background fetch+decode and returns `None` immediately -- same
    /// "paint nothing this frame, `cx.notify()` when it lands" shape as
    /// `ImageStore::get`.
    pub(crate) fn tile_for_ms(
        &self,
        meta: &TrickplayMeta,
        time_ms: u32,
        owner: WeakEntity<Root>,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        let (sheet, row, col) = locate(meta, time_ms)?;
        let mut guard = lock_ignore_poison(&self.inner);
        if let Some(cached) = guard.crops.get(&(sheet, row, col)) {
            return Some(cached.clone());
        }
        if let Some(full) = guard.sheets.get(&sheet).cloned() {
            let tw = meta.width;
            let th = meta.height;
            let x = col.checked_mul(tw)?;
            let y = row.checked_mul(th)?;
            let right = x.checked_add(tw)?;
            let bottom = y.checked_add(th)?;
            if right <= full.width() && bottom <= full.height() {
                let cropped = full.view(x, y, tw, th).to_image();
                let rendered = Arc::new(to_render_image(cropped));
                if guard.crops.len() >= MAX_CROPS {
                    if let Some(key) = guard.crops.keys().next().copied() {
                        guard.crops.remove(&key);
                    }
                }
                guard.crops.insert((sheet, row, col), rendered.clone());
                return Some(rendered);
            }
            return None;
        }
        if guard.inflight.len() >= MAX_INFLIGHT_SHEETS || !guard.inflight.insert(sheet) {
            return None; // already fetching this sheet.
        }
        let client = guard.client.clone();
        let tile_client = guard.tile_client.clone();
        let runtime = guard.runtime.clone();
        drop(guard);

        let item_id = meta.item_id.clone();
        let media_source_id = meta.media_source_id.clone();
        let width = meta.width;
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let url = client.trickplay_tile_url(&item_id, width, sheet, media_source_id.as_deref());
            let resp = tile_client
                .get(&url)
                .send()
                .await
                .ok()
                .filter(|r| r.status().is_success());
            let bytes = match resp {
                Some(resp) => read_capped(resp, TILE_BODY_CAP).await,
                None => None,
            };
            let decoded = bytes.and_then(|b| decode_bytes(&b));
            let _ = tx.send(decoded);
        });

        let inner = self.inner.clone();
        cx.spawn(async move |cx| {
            let decoded = rx.await.unwrap_or(None);
            if let Some(img) = decoded {
                let mut guard = lock_ignore_poison(&inner);
                insert_sheet(&mut guard, sheet, img);
                guard.inflight.remove(&sheet);
            } else {
                lock_ignore_poison(&inner).inflight.remove(&sheet);
            }
            let _ = owner.update(cx, |_root, cx| cx.notify());
        })
        .detach();

        None
    }
}

/// GEL-456: hard ceiling on one sprite-sheet response body -- same value and
/// reasoning as `media_cache`'s `IMAGE_BODY_CAP` (this fetch is deliberately
/// not routed through that cache, see the module doc comment, so it needs
/// its own bound). `Response::bytes()` buffers a whole body into memory
/// before anyone can look at it; a hostile or broken server answering a tile
/// request with gigabytes would be allocated in full.
const TILE_BODY_CAP: usize = 64 * 1024 * 1024;
const MAX_DECODED_SHEET_DIMENSION: u32 = 4_096;
const MAX_DECODED_SHEET_BYTES: usize = 64 * 1024 * 1024;
const MAX_SHEETS: usize = 12;
const MAX_SHEET_BYTES: usize = 192 * 1024 * 1024;
const MAX_CROPS: usize = 512;
const MAX_INFLIGHT_SHEETS: usize = 4;

/// Trickplay requests bypass the API client's request builder, so give them
/// equivalent transport limits. In particular, an HTTPS server must never
/// redirect a media request to cleartext HTTP.
fn trickplay_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let redirects_to_http = attempt.url().scheme() == "http"
                && attempt
                    .previous()
                    .last()
                    .is_some_and(|previous| previous.scheme() == "https");
            if redirects_to_http || attempt.previous().len() >= 10 {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        // Shared process-wide DNS cache (see jellyfin_api::dns's module
        // docs) -- scrub-preview fetches happen mid-playback, where a
        // resolver stall would delay the preview past the hover.
        .dns_resolver(jellyfin_api::dns::shared_dns_resolver())
        .build()
        .expect("trickplay HTTP client configuration is valid")
}

fn image_bytes(image: &image::RgbaImage) -> Option<usize> {
    usize::try_from(image.width())
        .ok()?
        .checked_mul(usize::try_from(image.height()).ok()?)?
        .checked_mul(4)
}

fn insert_sheet(inner: &mut Inner, sheet: u32, image: image::RgbaImage) {
    let Some(bytes) = image_bytes(&image) else {
        return;
    };
    if bytes > MAX_SHEET_BYTES {
        return;
    }
    if let Some(previous) = inner.sheets.remove(&sheet) {
        inner.sheet_bytes = inner
            .sheet_bytes
            .saturating_sub(image_bytes(&previous).unwrap_or(0));
    }
    while !inner.sheets.is_empty()
        && (inner.sheets.len() >= MAX_SHEETS
            || inner.sheet_bytes.saturating_add(bytes) > MAX_SHEET_BYTES)
    {
        if let Some(key) = inner.sheets.keys().next().copied() {
            if let Some(evicted) = inner.sheets.remove(&key) {
                inner.sheet_bytes = inner
                    .sheet_bytes
                    .saturating_sub(image_bytes(&evicted).unwrap_or(0));
            }
        }
    }
    // A replacement or eviction invalidates all pre-rendered crops from the
    // affected sheet; retaining them would defeat the cache bound.
    inner.crops.retain(|(cached_sheet, _, _), _| {
        *cached_sheet != sheet && inner.sheets.contains_key(cached_sheet)
    });
    inner.sheet_bytes = inner.sheet_bytes.saturating_add(bytes);
    inner.sheets.insert(sheet, Arc::new(image));
}

/// Reads at most `cap` bytes of `resp`'s body, chunk by chunk. `None` on any
/// transport error or on exceeding the cap -- the caller's existing
/// error-handling is already "no tile this frame", so a cap hit degrades to
/// exactly the same no-preview behavior as a failed fetch.
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Option<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > cap {
                    tracing::warn!(cap, "trickplay tile body exceeded the size cap; dropping");
                    return None;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Some(body),
            Err(e) => {
                tracing::warn!(error = %e, "trickplay tile fetch failed");
                return None;
            }
        }
    }
}

fn lock_ignore_poison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> TrickplayMeta {
        TrickplayMeta {
            item_id: "item".into(),
            media_source_id: None,
            width: 320,
            height: 180,
            tile_width: 10,
            tile_height: 10,
            interval_ms: 10_000,
            thumbnail_count: 1_000,
        }
    }

    #[test]
    fn locate_uses_checked_grid_arithmetic() {
        let mut invalid = meta();
        invalid.tile_width = u32::MAX;
        invalid.tile_height = 2;
        assert_eq!(locate(&invalid, 0), None);

        invalid = meta();
        invalid.interval_ms = 0;
        assert_eq!(locate(&invalid, 0), None);
    }

    #[test]
    fn locate_clamps_to_the_last_advertised_thumbnail() {
        let mut meta = meta();
        meta.thumbnail_count = 100;
        assert_eq!(locate(&meta, u32::MAX), Some((0, 9, 9)));
    }
}
