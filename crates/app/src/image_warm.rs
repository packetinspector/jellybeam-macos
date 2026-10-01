//! Background poster warming: trickle every library poster into the disk
//! image cache so a first browse paints real art instead of blurhashes.
//! Poster bytes are otherwise fetched lazily at display time (`cards.rs` ->
//! `image_store::ImageStore` -> `media_cache::ImageCache`).
//!
//! Calls the same `ImageCache::get` the render path calls, at the same
//! `POSTER_WIDTH` and `(item_id, kind, tag)` triple `cards.rs::
//! poster_art_source` resolves, so a warmed entry lands under the same cache
//! key the render asks for. Warmed bytes are never decoded into a
//! `RenderImage`: only disk bytes are pre-populated, decode stays on-demand.
//!
//! Politeness contract (must never slow foreground work):
//! 1. Strictly one warm fetch in flight -- `warm_pass` awaits each
//!    `ImageCache::get` before issuing the next, leaving at least three of
//!    the cache's four `IMAGE_FETCH_CONCURRENCY` permits for visible cells.
//! 2. Fully paused during playback via `set_playback_active`, driven from the
//!    same transitions as `Mirror::set_playback_active`.
//! 3. Skip-if-cached costs a `stat` (`ImageCache::is_cached_on_disk`), not a
//!    read, so a full rescan is cheap enough to re-run on every sync settle.
//!
//! Cancellation: a warm fetch runs at the current scroll/priority generation,
//! so a foreground `cancel_below_priority` can cancel it mid-flight --
//! `CacheError::Cancelled` is an ordinary skip, not an error. Warming never
//! calls `cancel_below_priority` itself, to avoid cancelling the foreground
//! fetches it exists to stay out of the way of.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use media_cache::{CacheError, CardRow, ImageCache, ImageKind, Mirror, Sort};

use crate::cards::{poster_art_source, PosterArtSource};
use crate::image_store::POSTER_WIDTH;

/// Breathing room between warm fetches: at one fetch in flight, a ~30KB
/// poster occupies a ~12Mbit link for ~20ms, so back-to-back requests would
/// hold the link continuously across a whole library.
const WARM_ITEM_DELAY: Duration = Duration::from_millis(50);

/// How often a paused pass re-checks whether playback ended. Polling (not a
/// `Notify`) keeps the check a plain atomic load with no wakeup plumbing; 2s
/// latency is irrelevant for a background trickle. Checked only *between*
/// fetches, so a play that starts mid-fetch still lets that poster finish.
const PLAYBACK_POLL: Duration = Duration::from_secs(2);

/// One poster to warm: the `(item_id, tag)` pair the rendered card asks
/// `ImageCache` for at `POSTER_WIDTH`. `item_id` is the id of whatever item
/// owns the art -- for a Season/Episode that's the series (see `warm_target`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WarmTarget {
    pub item_id: String,
    pub tag: String,
}

/// Warm order: the currently open library first, then the rest in sidebar
/// order. An `active_view_id` naming a view that no longer exists is
/// ignored -- the remaining order stays valid.
pub(crate) fn warm_view_order(
    views: &[media_cache::ViewSummary],
    active_view_id: Option<&str>,
) -> Vec<String> {
    let mut order: Vec<String> = Vec::with_capacity(views.len());
    if let Some(active) = active_view_id {
        if views.iter().any(|v| v.id == active) {
            order.push(active.to_string());
        }
    }
    for view in views {
        if Some(view.id.as_str()) != active_view_id {
            order.push(view.id.clone());
        }
    }
    order
}

/// The poster a `CardRow` actually paints, resolved through `cards.rs`'s own
/// fallback chain rather than a duplicate of it -- an Episode's poster slot
/// uses the series art, not its own still. `None` when nothing is warmable.
fn warm_target(row: &CardRow) -> Option<WarmTarget> {
    match poster_art_source(row) {
        PosterArtSource::Own(tag) => Some(WarmTarget {
            item_id: row.id.clone(),
            tag: tag.to_string(),
        }),
        PosterArtSource::SeriesFallback(series_id, tag) => Some(WarmTarget {
            item_id: series_id.to_string(),
            tag: tag.to_string(),
        }),
        PosterArtSource::None => None,
    }
}

/// Builds the full warm queue: every library's rows, active library first,
/// deduplicated (Season/Episode rows share one series poster). Pure apart
/// from `rows_for`, so ordering/dedup are testable without a mirror or GPUI.
pub(crate) fn build_warm_queue(
    views: &[media_cache::ViewSummary],
    active_view_id: Option<&str>,
    mut rows_for: impl FnMut(&str) -> Vec<CardRow>,
) -> Vec<WarmTarget> {
    let mut seen: HashSet<WarmTarget> = HashSet::new();
    let mut queue: Vec<WarmTarget> = Vec::new();
    for view_id in warm_view_order(views, active_view_id) {
        for row in rows_for(&view_id) {
            let Some(target) = warm_target(&row) else {
                continue;
            };
            if seen.insert(target.clone()) {
                queue.push(target);
            }
        }
    }
    queue
}

/// One scan's result: what the queue looked like, and the subset that isn't
/// on disk yet. The counts are what the pass logs.
struct WarmScan {
    /// Mirror rows walked. Logged alongside `queued` so "no artwork on this
    /// library" is distinguishable from "mirror still empty" (both are 0).
    scanned: usize,
    queued: usize,
    missing: Vec<WarmTarget>,
}

/// Blocking half of a pass: mirror reads (which check out a read-pool
/// connection) plus one `stat` per queued poster. Runs inside a single
/// `spawn_blocking`, never on a tokio worker or the GPUI thread.
fn scan_missing(mirror: &Mirror, cache: &ImageCache, active_view_id: Option<&str>) -> WarmScan {
    let views = mirror.views();
    let mut scanned = 0usize;
    let queue = build_warm_queue(&views, active_view_id, |view_id| {
        // Same query the library grid builds from (`root.rs::library_rows`).
        let rows = mirror
            .children_checked(view_id, Sort::NameAsc, 0, u32::MAX)
            .unwrap_or_default();
        scanned += rows.len();
        rows
    });
    let queued = queue.len();
    let missing = queue
        .into_iter()
        .filter(|t| !cache.is_cached_on_disk(&t.item_id, ImageKind::Primary, &t.tag, POSTER_WIDTH))
        .collect();
    WarmScan {
        scanned,
        queued,
        missing,
    }
}

/// Politeness rule 2: blocks while playback is active; returns immediately
/// (one atomic load) in the common idle case.
async fn wait_for_playback_idle(playback_active: &AtomicBool) {
    if !playback_active.load(Ordering::Acquire) {
        return;
    }
    tracing::debug!("poster warm paused: playback active");
    while playback_active.load(Ordering::Acquire) {
        tokio::time::sleep(PLAYBACK_POLL).await;
    }
    tracing::debug!("poster warm resumed: playback ended");
}

/// One full pass: scan, then fetch the misses one at a time.
async fn warm_pass(
    mirror: &Mirror,
    cache: &ImageCache,
    active_view_id: Option<String>,
    playback_active: &AtomicBool,
) {
    let started = Instant::now();
    let scan = {
        let mirror = mirror.clone();
        let cache = cache.clone();
        match tokio::task::spawn_blocking(move || {
            scan_missing(&mirror, &cache, active_view_id.as_deref())
        })
        .await
        {
            Ok(scan) => scan,
            Err(e) => {
                tracing::warn!(error = %e, "poster warm scan task failed");
                return;
            }
        }
    };

    if scan.missing.is_empty() {
        tracing::debug!(
            scanned = scan.scanned,
            queued = scan.queued,
            "poster warm pass: nothing to warm (every poster already cached)"
        );
        return;
    }
    tracing::info!(
        missing = scan.missing.len(),
        queued = scan.queued,
        scanned = scan.scanned,
        "poster warm pass starting"
    );

    let (mut fetched, mut skipped, mut cancelled, mut failed) = (0u64, 0u64, 0u64, 0u64);
    for target in scan.missing {
        wait_for_playback_idle(playback_active).await;
        // Re-probe: a multi-minute pass may have had this poster land since.
        if cache.is_cached_on_disk(
            &target.item_id,
            ImageKind::Primary,
            &target.tag,
            POSTER_WIDTH,
        ) {
            skipped += 1;
            continue;
        }
        match cache
            .get(
                &target.item_id,
                ImageKind::Primary,
                &target.tag,
                POSTER_WIDTH,
            )
            .await
        {
            Ok(_bytes) => {
                // Dropped deliberately: the point is the disk side effect.
                fetched += 1;
                tracing::debug!(item_id = %target.item_id, "poster warmed");
            }
            // Foreground cancel-below-priority won; not an error, retried next rescan.
            Err(CacheError::Cancelled) => {
                cancelled += 1;
                tracing::debug!(item_id = %target.item_id, "poster warm fetch cancelled by foreground scroll");
            }
            Err(e) => {
                failed += 1;
                tracing::debug!(item_id = %target.item_id, error = %e, "poster warm fetch failed");
            }
        }
        tokio::time::sleep(WARM_ITEM_DELAY).await;
    }

    tracing::info!(
        fetched,
        skipped,
        cancelled,
        failed,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "poster warm pass complete"
    );
}

/// Owns the background warm task for one session. Dropping ends the pass loop
/// once finished; callers needing it gone now (server switch, quit) call [`Self::abort`].
pub(crate) struct ImageWarmer {
    task: tokio::task::JoinHandle<()>,
    /// Capacity-1 "please (re)scan" mailbox carrying the library to warm
    /// first. A full channel means a pass is already queued and a full
    /// rescan is idempotent, so coalescing triggers costs nothing.
    trigger: tokio::sync::mpsc::Sender<Option<String>>,
    playback_active: Arc<AtomicBool>,
}

impl ImageWarmer {
    pub(crate) fn spawn(
        mirror: Mirror,
        cache: ImageCache,
        runtime: &tokio::runtime::Runtime,
    ) -> Self {
        let (trigger, mut rx) = tokio::sync::mpsc::channel::<Option<String>>(1);
        let playback_active = Arc::new(AtomicBool::new(false));
        let flag = playback_active.clone();
        let task = runtime.spawn(async move {
            while let Some(mut active_view_id) = rx.recv().await {
                // Collapse triggers piled up during the previous pass; newest wins.
                while let Ok(next) = rx.try_recv() {
                    active_view_id = next;
                }
                warm_pass(&mirror, &cache, active_view_id, &flag).await;
            }
        });
        Self {
            task,
            trigger,
            playback_active,
        }
    }

    /// Politeness rule 2. Called from `MainState::set_playback_active`,
    /// alongside the mirror's own breadth-sync yield.
    pub(crate) fn set_playback_active(&self, active: bool) {
        self.playback_active.store(active, Ordering::Release);
    }

    /// Requests a (re)scan, warming `active_view_id`'s posters first.
    /// Non-blocking and lossy by design: `trigger` coalesces bursts.
    pub(crate) fn request_pass(&self, active_view_id: Option<String>) {
        match self.trigger.try_send(active_view_id) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!("poster warm pass already queued; coalescing trigger");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("poster warm task is gone; ignoring trigger");
            }
        }
    }

    /// Session teardown/quit: the task holds a `Mirror` and an `ImageCache`
    /// clone for the old server and must not outlive it.
    pub(crate) fn abort(&self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_row(id: &str, item_type: &str) -> CardRow {
        CardRow {
            id: id.to_string(),
            item_type: item_type.to_string(),
            name: id.to_string(),
            primary_tag: None,
            blurhash: None,
            played: false,
            position_ticks: 0,
            runtime_ticks: None,
            unplayed_count: None,
            production_year: None,
            index_number: None,
            parent_index_number: None,
            series_id: None,
            series_primary_tag: None,
            parent_backdrop_item_id: None,
            parent_backdrop_tag: None,
            last_played_date: None,
            overview: None,
            premiere_date: None,
            is_virtual: false,
            series_name: None,
            library_id: None,
        }
    }

    fn movie(id: &str, tag: &str) -> CardRow {
        let mut row = base_row(id, "Movie");
        row.primary_tag = Some(tag.to_string());
        row
    }

    fn views() -> Vec<media_cache::ViewSummary> {
        vec![
            media_cache::ViewSummary {
                id: "movies".to_string(),
                name: "Movies".to_string(),
                kind: media_cache::ViewKind::Library,
            },
            media_cache::ViewSummary {
                id: "shows".to_string(),
                name: "TV Shows".to_string(),
                kind: media_cache::ViewKind::Library,
            },
            media_cache::ViewSummary {
                id: "music".to_string(),
                name: "Music Videos".to_string(),
                kind: media_cache::ViewKind::Library,
            },
        ]
    }

    #[test]
    fn view_order_is_sidebar_order_when_no_library_is_open() {
        assert_eq!(
            warm_view_order(&views(), None),
            vec!["movies", "shows", "music"]
        );
    }

    /// Pins: the active library is warmed first, every other library exactly
    /// once.
    #[test]
    fn the_active_library_is_warmed_first_and_not_twice() {
        assert_eq!(
            warm_view_order(&views(), Some("music")),
            vec!["music", "movies", "shows"]
        );
    }

    /// Pins: a stale active id doesn't inject a phantom view or drop real ones.
    #[test]
    fn an_unknown_active_library_falls_back_to_plain_sidebar_order() {
        assert_eq!(
            warm_view_order(&views(), Some("deleted")),
            vec!["movies", "shows", "music"]
        );
    }

    #[test]
    fn queue_follows_the_active_library_first_ordering() {
        let queue = build_warm_queue(&views(), Some("shows"), |view_id| match view_id {
            "movies" => vec![movie("m1", "mtag")],
            "shows" => vec![movie("s1", "stag")],
            _ => Vec::new(),
        });
        assert_eq!(
            queue,
            vec![
                WarmTarget {
                    item_id: "s1".to_string(),
                    tag: "stag".to_string()
                },
                WarmTarget {
                    item_id: "m1".to_string(),
                    tag: "mtag".to_string()
                },
            ]
        );
    }

    /// Pins: dedup collapses repeated active-library visits and shared
    /// series posters.
    #[test]
    fn duplicate_targets_are_queued_once() {
        let mut ep1 = base_row("ep1", "Episode");
        ep1.primary_tag = Some("still-1".to_string());
        ep1.series_id = Some("series-9".to_string());
        ep1.series_primary_tag = Some("series-poster".to_string());
        let mut ep2 = base_row("ep2", "Episode");
        ep2.primary_tag = Some("still-2".to_string());
        ep2.series_id = Some("series-9".to_string());
        ep2.series_primary_tag = Some("series-poster".to_string());

        let queue = build_warm_queue(&views(), Some("shows"), |view_id| match view_id {
            "shows" => vec![ep1.clone(), ep2.clone(), movie("m1", "mtag")],
            _ => Vec::new(),
        });

        assert_eq!(
            queue,
            vec![
                // Both resolve to the series poster, not their own still
                // (docs/DESIGN-PLAYER-NAV.md §2.5).
                WarmTarget {
                    item_id: "series-9".to_string(),
                    tag: "series-poster".to_string()
                },
                WarmTarget {
                    item_id: "m1".to_string(),
                    tag: "mtag".to_string()
                },
            ]
        );
    }

    /// Pins: the same item in two libraries queues once.
    #[test]
    fn the_same_poster_in_two_libraries_is_queued_once() {
        let queue = build_warm_queue(&views(), None, |_| vec![movie("m1", "mtag")]);
        assert_eq!(queue.len(), 1);
    }

    /// Pins: a differing tag is a different cache key (the server's change
    /// signal), not deduped.
    #[test]
    fn a_changed_tag_for_the_same_item_is_a_separate_target() {
        let queue = build_warm_queue(&views(), None, |view_id| match view_id {
            "movies" => vec![movie("m1", "old-tag")],
            "shows" => vec![movie("m1", "new-tag")],
            _ => Vec::new(),
        });
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn rows_with_no_art_anywhere_in_the_chain_are_skipped() {
        let queue = build_warm_queue(&views(), None, |view_id| match view_id {
            "movies" => vec![base_row("no-art", "Movie"), movie("m1", "mtag")],
            _ => Vec::new(),
        });
        assert_eq!(
            queue,
            vec![WarmTarget {
                item_id: "m1".to_string(),
                tag: "mtag".to_string()
            }]
        );
    }

    #[test]
    fn an_empty_mirror_yields_an_empty_queue() {
        assert!(build_warm_queue(&[], None, |_| Vec::new()).is_empty());
        assert!(build_warm_queue(&views(), None, |_| Vec::new()).is_empty());
    }
}
