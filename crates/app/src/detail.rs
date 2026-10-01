//! Detail page: backdrop + poster + metadata line + Play/Resume + overview;
//! for a Series, seasons → episodes inline (docs/UX-SPEC.md §5).
//!
//! Two data sources, both cache-first (docs/DESIGN-PLAYER-NAV.md §2.7):
//! `Mirror::item()` (docs/DATA.md: the one browse-adjacent path allowed to parse
//! the blob) paints the item instantly, and `Mirror::children()` under
//! `Sort::IndexNumber` paints a Series' seasons and the selected season's
//! episodes just as instantly. The only live network fetch left in this
//! module is `fetch_media_streams` (Movie/Episode `MediaStreams` codec
//! badges, which the mirror's bulk sync doesn't carry), which enriches
//! `DetailState::dto` in place once it lands.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    div, img, linear_color_stop, linear_gradient, point, prelude::*, px, rgb, rgba, svg,
    AnimationExt, AnyElement, App, Context, Div, ObjectFit, ScrollHandle, SharedString, Stateful,
    StyledImage, WeakEntity,
};
use jellyfin_api::models::{BaseItemDto, BaseItemKind, LocationType, PersonKind};
use jellyfin_api::{ItemQuery, JellyfinClient};
use media_cache::{CardRow, ImageKind, Mirror, Sort};

use crate::cards::{display_title, episode_card, format_runtime, poster_card};
use crate::focus_grid::GridFocus;
use crate::grid::{columns_for_width, CELL_GAP, CELL_WIDTH};
use crate::image_store::{ImageStore, BACKDROP_WIDTH, PORTRAIT_WIDTH, POSTER_WIDTH};
use crate::root::{PreloadTrigger, Root, Screen};
use crate::scroll_axis::{Axis, AxisLock};
use crate::theme;
use crate::ui::components::{
    button as button_component, clamped_line, edge_faded_strip, focus_ring, ButtonSize,
    ButtonVariant,
};
use crate::ui::popover::{click_away_catcher, popover_panel, popover_trigger};
use crate::ui::spec_strip::{self, media_breakdown, spec_info_button, MediaFacts};

/// How many seasons/episodes a single `Mirror::children()` call pulls at
/// once -- generous enough for any real show without being unbounded.
const SEASONS_LIMIT: u32 = 200;
const EPISODES_LIMIT: u32 = 500;
/// How many `get_similar` results to ask for (docs/UX-SPEC.md §5 "Similar titles
/// row") -- enough to fill the row without over-fetching.
const SIMILAR_LIMIT: u32 = 16;

/// One `episode_card` row's on-screen height, computed not guessed:
/// `cards.rs`'s fixed per-card height (`TITLE_BLOCK_HEIGHT`=36px,
/// `SYNOPSIS_BLOCK_HEIGHT`=32px, duplicated here as literals) atop the 16:9
/// art (`CELL_WIDTH * 9/16`), plus a 4px `gap_1()` and this module's
/// `.pb(px(CELL_GAP))`. Used only by `render_seasons_children`'s min-height
/// spacer; see ARCHITECTURE.md's "Detail episode grid width" entry for why
/// no live layout measurement is available here.
const EPISODE_ROW_HEIGHT: f32 = CELL_WIDTH * 9.0 / 16.0 + 4.0 + 68.0 + CELL_GAP;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailArea {
    Play,
    Seasons,
    Episodes,
}

pub(crate) struct DetailState {
    pub item_id: String,
    pub dto: Option<BaseItemDto>,
    pub is_series: bool,
    /// True when `dto`'s type is `Episode` (docs/DESIGN-PLAYER-NAV.md Part
    /// 2, Episode Detail page). Drives `render`'s dispatch to
    /// `render_episode` and `move_focus`'s Play->Down transition straight to
    /// the sibling rail (no season-tabs stop).
    pub is_episode: bool,
    pub seasons: Vec<CardRow>,
    pub selected_season: usize,
    /// For a Series: the selected season's episodes. For an Episode: the
    /// *sibling* episodes of the same season -- one `DetailState` is never
    /// both at once, so this one field covers both shapes.
    pub episodes: Vec<CardRow>,
    pub area: DetailArea,
    pub episode_focus: GridFocus,
    /// §2.3: series-scoped "Up Next"/"Resume" CTA, computed once at `load`
    /// time and refreshed by `session.rs::on_mirror_change`; see
    /// `find_series_next_episode`'s doc comment for the derivation. `None`
    /// for a Movie, or a fully-watched series.
    pub next_episode: Option<CardRow>,
    /// docs/UX-SPEC.md §5 "Similar titles row": populated by a live `get_similar`
    /// fetch (`Root::spawn_similar`), not the mirror (no local
    /// recommendation index). Empty until that fetch lands, for an Episode
    /// (skipped), or if the server returns nothing.
    pub similar: Vec<CardRow>,
    /// Tracks `#detail-scroll`'s scroll state, persisted across renders like
    /// `home.rs::HomeState::shelf_scrolls`. `detail.rs::move_focus` calls
    /// `.scroll_to_item(scroll_target_index())` on it after every arrow-key
    /// focus move; see `scroll_target_index`'s doc comment for why the
    /// episode rows must be direct children of the tracked container.
    pub scroll: ScrollHandle,
    /// Is the spec strip's ⓘ breakdown popover open? Page-scoped since a
    /// Detail page only ever renders one strip. Reset to `false` by
    /// `DetailState::new` so navigating to another item never inherits it.
    pub media_info_open: bool,
    /// The season-tab strip's own horizontal scroll state, persisted for
    /// trackpad scrolling, `ScrollHandle::scroll_to_item` on keyboard season
    /// select, and `edge_fade_overlays`' overflow detection (the only
    /// overflow signal this gpui version exposes to app code).
    pub season_scroll: ScrollHandle,
    /// Same treatment for the cast rail and the similar-titles rail, so
    /// their edge fades appear only on the edge with content beyond it.
    pub cast_scroll: ScrollHandle,
    pub similar_scroll: ScrollHandle,
    /// One `AxisLock` per horizontal rail on this page, the same per-surface
    /// treatment `home.rs::HomeState::shelf_axis_locks` gives each shelf --
    /// see `RailAxisLocks` and `scroll_axis.rs`'s module doc comment.
    pub axis_locks: RailAxisLocks,
}

/// The detail page's horizontal rails, each with its own gesture state.
///
/// One lock per rail rather than one per page: a lock is *gesture* state
/// (which axis the current swipe owns), and two rails must never inherit
/// each other's mid-gesture decision.
///
/// `Rc<RefCell<..>>` because the lock is *moved* into an `on_scroll_wheel`
/// closure that outlives the render call, while `DetailState` keeps its own
/// handle across renders.
///
/// The episode grid (series page) and sibling grid (episode page) are
/// deliberately absent: both are vertical grids with no `overflow_x_scroll`
/// of their own, so there is no horizontal scroll for a gesture to fight
/// over.
#[derive(Debug, Clone)]
pub(crate) struct RailAxisLocks {
    pub seasons: Rc<RefCell<AxisLock>>,
    pub cast: Rc<RefCell<AxisLock>>,
    pub similar: Rc<RefCell<AxisLock>>,
}

impl Default for RailAxisLocks {
    fn default() -> Self {
        RailAxisLocks {
            seasons: Rc::new(RefCell::new(AxisLock::new())),
            cast: Rc::new(RefCell::new(AxisLock::new())),
            similar: Rc::new(RefCell::new(AxisLock::new())),
        }
    }
}

/// Wires one horizontally-scrolling rail to its own `AxisLock`, the same
/// treatment `home.rs::render_shelf` applies to a shelf (see
/// `scroll_axis.rs`'s module doc comment for why). Vertical-dominant
/// gestures scroll the page and leave the rail still; horizontal-dominant
/// ones scroll the rail and never reach `#detail-scroll`'s own listener.
fn axis_locked_rail(
    strip: Stateful<Div>,
    scroll: &ScrollHandle,
    axis_lock: Rc<RefCell<AxisLock>>,
) -> Stateful<Div> {
    let scroll = scroll.clone();
    strip.on_scroll_wheel(move |event, window, cx| {
        let delta = event.delta.pixel_delta(window.line_height());
        let axis =
            axis_lock
                .borrow_mut()
                .on_event(f32::from(delta.x), f32::from(delta.y), Instant::now());
        match axis {
            Axis::Horizontal => {
                // gpui's own listener already applied `delta.x` to this
                // rail; all that's left is to keep the residual `delta.y`
                // from also nudging the page behind it.
                cx.stop_propagation();
            }
            Axis::Vertical => {
                // Undo gpui's single-axis fallback (a plain mouse wheel has
                // `delta.x == 0`, so it reappropriates `delta.y` as this
                // rail's horizontal delta before our handler ever runs),
                // leaving the event unconsumed for the page to scroll on.
                let applied_dx = if delta.x != px(0.) { delta.x } else { delta.y };
                if applied_dx != px(0.) {
                    let current = scroll.offset();
                    scroll.set_offset(point(current.x - applied_dx, current.y));
                }
            }
        }
    })
}

/// §2.3's series-scoped "Up Next"/"Resume" CTA: walk this series' seasons/
/// episodes (all local `Mirror::children()` reads, zero network) in order
/// and return the first one that's in-progress ("Resume" beats "Up Next") or
/// unwatched. `None` once every episode is watched.
pub(crate) fn find_series_next_episode(mirror: &Mirror, seasons: &[CardRow]) -> Option<CardRow> {
    find_next_episode_in(
        seasons_in_watch_order(seasons)
            .into_iter()
            .map(|season| mirror.children(&season.id, Sort::IndexNumber, 0, EPISODES_LIMIT)),
    )
}

/// The order seasons are walked in when answering "what's next". A Jellyfin
/// "Specials" season is number **0**, so `Mirror::children`'s ascending sort
/// leads with it; this reorders Specials to *after* every regular season so
/// they don't pre-empt the story but stay reachable as a last resort once
/// everything else is watched. Browse lists are unaffected -- this is only
/// the next-up/auto-advance ordering.
pub(crate) fn seasons_in_watch_order(seasons: &[CardRow]) -> Vec<&CardRow> {
    let (specials, regular): (Vec<&CardRow>, Vec<&CardRow>) =
        seasons.iter().partition(|s| s.index_number == Some(0));
    regular.into_iter().chain(specials).collect()
}

/// The season-list walk `find_series_next_episode` performs, split out so it
/// can be tested headlessly (no item-injection seam on a real `Mirror`):
/// returns the in-progress episode if there is one, else the first
/// unwatched one.
pub(crate) fn find_next_episode_in(
    seasons_episodes: impl IntoIterator<Item = Vec<CardRow>>,
) -> Option<CardRow> {
    let mut first_unwatched = None;
    for episodes in seasons_episodes {
        for ep in episodes {
            // A virtual (unaired/missing) episode has no media and is never
            // `played`, so it would otherwise win "first unwatched" on a
            // currently-airing show and get offered as an unplayable CTA.
            // It stays visible in browse/episode lists; only auto-select
            // excludes it.
            if ep.is_virtual {
                continue;
            }
            if ep.position_ticks > 0 {
                return Some(ep);
            }
            if !ep.played && first_unwatched.is_none() {
                first_unwatched = Some(ep);
            }
        }
    }
    first_unwatched
}

/// `(is_series, is_episode)` for `DetailState::load`'s layout branch -- a
/// Series gets the seasons/episodes layout, an Episode gets its own hero
/// (`render_episode`), and everything else (`Movie`, and per
/// docs/PLUGIN-CHANNELS.md §2.4, `Video`/`MusicVideo`/a
/// Live-TV `Recording`) falls through to the generic Movie-shaped layout
/// with the ordinary Play/Resume primary action. There is no separate
/// "which item types get a Play button" gate: any type that isn't Series or
/// Episode gets one for free. Extracted so this is pinned by a unit test.
fn detail_layout_kind(item_type: Option<BaseItemKind>) -> (bool, bool) {
    let is_series = matches!(item_type, Some(BaseItemKind::Series));
    let is_episode = matches!(item_type, Some(BaseItemKind::Episode));
    (is_series, is_episode)
}

impl DetailState {
    /// `#detail-scroll`'s child index of the season-tabs row on a Series
    /// page -- child 0 is the hero block, child 1 the season-tabs row,
    /// children 2.. the episode rows (matches `scroll_target_index`'s
    /// `prefix`). `detail.rs::select_season` targets this index explicitly on
    /// every season switch to keep the scroll position stable across a
    /// season's episode count changing.
    pub(crate) const SEASON_TABS_CHILD_INDEX: usize = 1;

    /// Instant paint from the mirror only, seasons/episodes included.
    /// `MediaStreams` (Movie/Episode) is still backfilled live by
    /// `root.rs`'s `spawn_detail_enrichment` right after this.
    pub(crate) fn load(mirror: &Mirror, item_id: String) -> Self {
        let dto = mirror.item(&item_id);
        let item_type = dto.as_ref().and_then(|d| d.type_);
        let (is_series, is_episode) = detail_layout_kind(item_type);

        let seasons = if is_series {
            mirror.children(&item_id, Sort::IndexNumber, 0, SEASONS_LIMIT)
        } else {
            Vec::new()
        };
        // Best-effort: a DTO missing `SeasonId` just leaves the sibling rail
        // empty rather than panicking.
        let episodes = if is_series {
            seasons
                .first()
                .map(|s| mirror.children(&s.id, Sort::IndexNumber, 0, EPISODES_LIMIT))
                .unwrap_or_default()
        } else if is_episode {
            dto.as_ref()
                .and_then(|d| d.season_id)
                .map(|sid| mirror.children(&sid.to_string(), Sort::IndexNumber, 0, EPISODES_LIMIT))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut episode_focus = GridFocus::new(1);
        episode_focus.clamp(episodes.len());
        let next_episode = if is_series {
            find_series_next_episode(mirror, &seasons)
        } else {
            None
        };
        DetailState {
            item_id,
            dto,
            is_series,
            is_episode,
            seasons,
            selected_season: 0,
            episodes,
            area: DetailArea::Play,
            episode_focus,
            next_episode,
            similar: Vec::new(),
            scroll: ScrollHandle::new(),
            media_info_open: false,
            season_scroll: ScrollHandle::new(),
            cast_scroll: ScrollHandle::new(),
            similar_scroll: ScrollHandle::new(),
            axis_locks: RailAxisLocks::default(),
        }
    }

    /// Which direct child of `#detail-scroll`'s tracked container
    /// `move_focus` should scroll into view for the current
    /// `area`/`episode_focus`. `Play`/`Seasons` target the top (index 0);
    /// `Episodes` targets the focused episode's row, offset by however many
    /// fixed children (hero block, and -- Series only -- the season-tabs
    /// row) precede the episode rows -- those rows must be direct children
    /// rather than nested in a wrapper `div` for this to address them
    /// individually.
    pub(crate) fn scroll_target_index(&self) -> usize {
        match self.area {
            DetailArea::Play | DetailArea::Seasons => 0,
            DetailArea::Episodes => {
                let prefix = if self.is_episode {
                    // hero/breadcrumb block, then siblings-header row.
                    2
                } else if self.is_series {
                    // hero block, then the season-tabs row.
                    2
                } else {
                    // A movie has no episode grid; unreachable in practice,
                    // but degrade to "top" rather than an out-of-range index.
                    return 0;
                };
                prefix + self.episode_focus.row()
            }
        }
    }

    pub(crate) fn focused_episode(&self) -> Option<&CardRow> {
        self.episodes.get(self.episode_focus.index)
    }

    /// docs/UX-SPEC.md §2 arrow-key nav for the Detail page: Play button → (Series
    /// only) season tabs → episode grid, top-to-bottom. Returns
    /// `Some(season_index)` when Left/Right on the season tabs should
    /// trigger `detail.rs::select_season`, which is owned by `detail.rs` since
    /// it needs `&mut MainState`.
    pub(crate) fn move_focus(&mut self, dir: crate::focus_grid::Direction) -> Option<usize> {
        use crate::focus_grid::Direction;
        match (self.area, dir) {
            (DetailArea::Play, Direction::Down) => {
                // An Episode page has no season tabs -- Down goes straight
                // to the sibling rail.
                self.area = if self.is_series {
                    DetailArea::Seasons
                } else if !self.episodes.is_empty() {
                    DetailArea::Episodes
                } else {
                    DetailArea::Play
                };
                None
            }
            (DetailArea::Seasons, Direction::Up) => {
                self.area = DetailArea::Play;
                None
            }
            (DetailArea::Seasons, Direction::Down) => {
                if !self.episodes.is_empty() {
                    self.area = DetailArea::Episodes;
                }
                None
            }
            (DetailArea::Seasons, Direction::Left) => {
                (self.selected_season > 0).then(|| self.selected_season - 1)
            }
            (DetailArea::Seasons, Direction::Right) => {
                (self.selected_season + 1 < self.seasons.len()).then(|| self.selected_season + 1)
            }
            (DetailArea::Episodes, Direction::Up) if self.episode_focus.row() == 0 => {
                self.area = if self.seasons.is_empty() {
                    DetailArea::Play
                } else {
                    DetailArea::Seasons
                };
                None
            }
            (DetailArea::Episodes, dir) => {
                let n = self.episodes.len();
                match dir {
                    Direction::Left => self.episode_focus.left(n),
                    Direction::Right => self.episode_focus.right(n),
                    Direction::Up => self.episode_focus.up(n),
                    Direction::Down => self.episode_focus.down(n),
                }
                None
            }
            _ => None,
        }
    }
}

/// Live `Fields=MediaStreams` fetch, run once per Detail visit for
/// Movie/Episode items (Series/Season DTOs never carry their own streams).
/// Fire-and-forget from `Root::open_detail`; updates `DetailState::dto` in
/// place and `cx.notify()`s when it lands.
pub(crate) async fn fetch_media_streams(
    client: JellyfinClient,
    item_id: String,
) -> Option<BaseItemDto> {
    let query = ItemQuery {
        ids: vec![item_id],
        fields: vec![
            "MediaStreams".to_string(),
            "MediaSources".to_string(),
            "Overview".to_string(),
            "Genres".to_string(),
            "People".to_string(),
            // Chapter ticks + trickplay preview (player OSD) also need
            // fields the mirror's bulk sync doesn't carry; reuse this fetch
            // rather than adding a second one from `playback.rs`.
            "Chapters".to_string(),
            "Trickplay".to_string(),
        ],
        limit: 1,
        ..ItemQuery::new()
    };
    match client.get_items(&query).await {
        Ok(result) => result.items.into_iter().next(),
        Err(e) => {
            tracing::debug!(error = %e, "detail media-streams enrichment fetch failed");
            None
        }
    }
}

/// Grafts just the enrichment-only fields (the ones *only* a live per-item
/// fetch ever populates: `MediaStreams`, `MediaSources`, `Chapters`,
/// `Trickplay`, `People`, `Overview`, `Genres`) across a dto swap, in
/// whichever direction it runs: adopt `fresh` for every other field, but for
/// each of those seven, only take `fresh`'s value when `fresh` actually
/// carries one -- otherwise keep whatever `base` already had. This avoids
/// wholesale struct replacement wiping fields a partial fetch or the
/// mirror's own blob (which never carries those seven) doesn't carry, while
/// every other field still updates to the newer dto.
///
/// Doesn't touch which *image* the page renders, on purpose -- that's
/// `DetailState`'s separately pinned art-source field, not something a
/// field-merge rule alone can guarantee stays stable.
pub(crate) fn merge_enrichment(base: &mut BaseItemDto, fresh: BaseItemDto) {
    let media_streams = if fresh.media_streams.is_empty() {
        std::mem::take(&mut base.media_streams)
    } else {
        Vec::new() // overwritten below by `fresh`'s own value.
    };
    let media_sources = if fresh.media_sources.is_empty() {
        std::mem::take(&mut base.media_sources)
    } else {
        Vec::new()
    };
    let chapters = if fresh.chapters.is_empty() {
        std::mem::take(&mut base.chapters)
    } else {
        Vec::new()
    };
    let trickplay = if fresh.trickplay.is_empty() {
        std::mem::take(&mut base.trickplay)
    } else {
        Default::default()
    };
    let people = if fresh.people.is_empty() {
        std::mem::take(&mut base.people)
    } else {
        Vec::new()
    };
    let overview = if fresh.overview.is_none() {
        base.overview.take()
    } else {
        None
    };
    let genres = if fresh.genres.is_empty() {
        std::mem::take(&mut base.genres)
    } else {
        Vec::new()
    };

    *base = fresh;

    if base.media_streams.is_empty() {
        base.media_streams = media_streams;
    }
    if base.media_sources.is_empty() {
        base.media_sources = media_sources;
    }
    if base.chapters.is_empty() {
        base.chapters = chapters;
    }
    if base.trickplay.is_empty() {
        base.trickplay = trickplay;
    }
    if base.people.is_empty() {
        base.people = people;
    }
    if base.overview.is_none() {
        base.overview = overview;
    }
    if base.genres.is_empty() {
        base.genres = genres;
    }
}

#[cfg(test)]
mod merge_enrichment_tests {
    use super::*;
    use crate::test_support::dto_from;

    /// Pins: a complete fresh dto grafts enrichment fields and every other
    /// field onto `base`.
    #[test]
    fn merge_enrichment_grafts_enrichment_fields_from_a_complete_fresh_dto() {
        let mut base = dto_from(serde_json::json!({"Name": "Old Name"}));
        let fresh = dto_from(serde_json::json!({
            "Name": "New Name",
            "Overview": "A synopsis.",
            "Genres": ["Drama"],
        }));
        merge_enrichment(&mut base, fresh);
        assert_eq!(base.name.as_deref(), Some("New Name"));
        assert_eq!(base.overview.as_deref(), Some("A synopsis."));
        assert_eq!(base.genres, vec!["Drama".to_string()]);
    }

    /// Pins: a fresh dto lacking enrichment fields keeps `base`'s prior
    /// values while other fields still update.
    #[test]
    fn merge_enrichment_preserves_prior_enrichment_when_fresh_lacks_it() {
        let mut base = dto_from(serde_json::json!({
            "Name": "Old Name",
            "Overview": "A synopsis.",
            "Genres": ["Drama"],
        }));
        // No Overview/Genres -- the mirror never carries them.
        let fresh = dto_from(serde_json::json!({"Name": "New Name (mirror)"}));
        merge_enrichment(&mut base, fresh);
        assert_eq!(base.name.as_deref(), Some("New Name (mirror)"));
        assert_eq!(base.overview.as_deref(), Some("A synopsis."));
        assert_eq!(base.genres, vec!["Drama".to_string()]);
    }

    #[test]
    fn merge_enrichment_keeps_media_streams_when_fresh_has_none() {
        let mut base = dto_from(serde_json::json!({"MediaStreams": [{}]}));
        let fresh = dto_from(serde_json::json!({}));
        merge_enrichment(&mut base, fresh);
        assert_eq!(base.media_streams.len(), 1);
    }

    #[test]
    fn merge_enrichment_replaces_media_streams_when_fresh_has_some() {
        let mut base = dto_from(serde_json::json!({"MediaStreams": [{}]}));
        let fresh = dto_from(serde_json::json!({"MediaStreams": [{}, {}]}));
        merge_enrichment(&mut base, fresh);
        assert_eq!(base.media_streams.len(), 2);
    }
}

/// docs/UX-SPEC.md §5 "Similar titles row" -- `GET /Items/{itemId}/Similar`.
/// Fire-and-forget from `Root::spawn_similar`; a server error or a server
/// with no recommendations both degrade to an empty `Vec` rather than an
/// error banner, since `render`'s `similar_row` already renders nothing when
/// the list is empty.
pub(crate) async fn fetch_similar(client: JellyfinClient, item_id: String) -> Vec<BaseItemDto> {
    match client.get_similar(&item_id, SIMILAR_LIMIT).await {
        Ok(items) => items,
        Err(e) => {
            tracing::debug!(error = %e, "detail similar-items fetch failed");
            Vec::new()
        }
    }
}

/// Adapts a `get_similar` result (`BaseItemDto`, straight off the wire) into
/// the `CardRow` shape `cards::poster_card` renders, so `similar_row` reuses
/// the same cell component as the Library grid/Home shelves/episode rails.
/// Deliberately narrow: only the fields `poster_card`/`poster_art_source`/
/// `badges` read are populated; this is not a general DTO->CardRow mapper
/// (`media_cache::rows::extract_columns` is, and is crate-private) and is
/// never written back to the mirror.
pub(crate) fn card_row_from_dto(dto: &BaseItemDto) -> Option<CardRow> {
    let id = dto.id?.to_string();
    let user_data = dto.user_data.as_ref();
    Some(CardRow {
        id,
        item_type: dto.type_.map(|t| t.to_string()).unwrap_or_default(),
        name: dto.name.clone().unwrap_or_default(),
        primary_tag: dto.image_tags.get("Primary").cloned(),
        blurhash: dto.image_tags.get("Primary").and_then(|tag| {
            dto.image_blur_hashes
                .as_ref()
                .and_then(|h| h.primary.get(tag).cloned())
        }),
        played: user_data.and_then(|u| u.played).unwrap_or(false),
        position_ticks: user_data
            .and_then(|u| u.playback_position_ticks)
            .unwrap_or(0),
        runtime_ticks: dto.run_time_ticks,
        unplayed_count: user_data.and_then(|u| u.unplayed_item_count).map(i64::from),
        production_year: dto.production_year,
        index_number: dto.index_number,
        parent_index_number: dto.parent_index_number,
        series_id: dto.series_id.map(|u| u.to_string()),
        series_primary_tag: dto.series_primary_image_tag.clone(),
        parent_backdrop_item_id: dto.parent_backdrop_item_id.map(|u| u.to_string()),
        parent_backdrop_tag: dto.parent_backdrop_image_tags.first().cloned(),
        last_played_date: None,
        overview: dto.overview.clone(),
        premiere_date: dto.premiere_date.map(|d| d.to_rfc3339()),
        is_virtual: dto.location_type == Some(LocationType::Virtual),
        series_name: dto.series_name.clone(),
        // Similar-rail cards never feed Home's per-library visibility
        // filter, so there's no need to resolve one here.
        library_id: None,
    })
}

/// The Play/Resume button shared by the Movie/Series layout (`render`) and
/// the Episode Detail page (`render_episode`), so both get identical label
/// logic and focus-ring treatment.
///
/// `virtual_reason`: `Some(reason)` (`"Airs <date>"`/`"Missing"`, from
/// `cards::virtual_status_label`) when the item is virtual (unaired/missing,
/// no `MediaSources` to play). `offline` wins if both are true -- the user
/// needs to know "your server connection", not "this episode", first.
///
/// `resume` carries not just *that* there is watch progress but
/// *what* the resume target is -- for a Series page, the in-progress
/// episode -- so the primary button can say `Resume S2 E6` and actually
/// play that episode.
#[allow(clippy::too_many_arguments)]
fn play_button_element(
    item_id: String,
    item_name: String,
    resume: Option<ResumeTarget>,
    playing_this_item: bool,
    paused: bool,
    focused: bool,
    root: WeakEntity<Root>,
    offline: bool,
    virtual_reason: Option<String>,
) -> AnyElement {
    let play_label = primary_play_label(
        playing_this_item,
        paused,
        resume.as_ref().and_then(|r| r.episode_label.as_deref()),
        resume.is_some(),
    );
    // Plays the resume target when there is one, otherwise this page's own
    // item.
    let (target_id, target_name) = match &resume {
        Some(r) => (r.item_id.clone(), r.item_name.clone()),
        None => (item_id, item_name),
    };
    let start_over_root = root.clone();

    // Disabled with a reason proactively (docs/UX-SPEC.md §6), rather than letting
    // the click land and only then surfacing `play_item`'s error banner.
    let disabled_reason = if offline {
        Some("Server offline".to_string())
    } else {
        virtual_reason
    };
    let disabled = disabled_reason.is_some();

    // `min_w` not a fixed `w` -- "Resume S2 E6" doesn't fit 140px,
    // and a clipped label is worse than a wider button. Short labels still
    // render at the old 140px.
    let button = div()
        .id("play-button")
        .relative()
        .mt_2()
        .min_w(px(140.))
        // Brand §5's Primary button: padding 11x22, fully rounded 999px,
        // PISTACCHIO fill with a NOTTE label in Archivo 700 15px.
        .px(px(22.))
        .h(px(44.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_PILL)
        .font_weight(gpui::FontWeight::BOLD)
        .when(disabled, |d| d.bg(rgb(theme::SURFACE_OVERLAY)))
        .when(!disabled, |d| d.bg(rgb(theme::PRIMARY_BUTTON_BG)))
        .text_color(if disabled {
            rgba(theme::TEXT_TERTIARY)
        } else {
            rgb(theme::PRIMARY_BUTTON_TEXT)
        })
        .when(!disabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.bg(rgb(theme::PRIMARY_BUTTON_BG_HOVER)))
                .active(|s| s.bg(rgb(theme::PRIMARY_BUTTON_BG_PRESSED)))
        })
        // Unified focus treatment: the same 2px/2px-offset PISTACCHIO ring
        // the episode rail uses, not a flush spread shadow -- see
        // `ui::components::focus_ring`.
        .when(focused, |d| d.child(focus_ring(theme::RADIUS_PILL)))
        .child(SharedString::from(play_label))
        .when(!disabled, |d| {
            let click_id = target_id.clone();
            let click_name = target_name.clone();
            d.on_click(move |_event, _window, cx| {
                let _ = root.update(cx, |root, cx| {
                    root.play_item(click_id.clone(), click_name.clone(), cx)
                });
            })
        });

    // "Start from beginning": exists only when there is progress to start
    // over from. Brand §5 Secondary button (transparent pill, 1px HAIRLINE
    // border, PANNA-2 label), same `lg` box as the Primary beside it, laid
    // out in a row so it adds no height to the hero text column.
    let start_over = (resume.is_some() && !disabled).then(|| {
        let root = start_over_root;
        button_component(
            "play-from-start-button",
            "Start from beginning",
            ButtonVariant::Secondary,
            ButtonSize::Lg,
            false,
        )
        .mt_2()
        .on_click(move |_event, _window, cx| {
            let _ = root.update(cx, |root, cx| {
                root.play_item_from_start(target_id.clone(), target_name.clone(), cx)
            });
        })
    });

    let action_row = div()
        .flex()
        .flex_row()
        // The hero text column is a fraction of the window (left 46%), so on
        // a narrow window Primary+Secondary can run wider than it; wrapping
        // keeps either from overflowing the scrimmed band onto raw artwork.
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(button)
        .children(start_over);

    if let Some(reason) = disabled_reason {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(action_row)
            .child(
                div()
                    .text_xs()
                    .text_color(rgba(theme::TEXT_TERTIARY))
                    .child(reason),
            )
            .into_any_element()
    } else {
        action_row.into_any_element()
    }
}

/// The primary-action label, pure so the Play/Resume decision is
/// testable without a GPUI window. `resume_episode` is the `"S2 E6"` tag
/// when resume data points at a specific episode (Series page), `None` for
/// the page's own item (Movie/Episode page); `has_resume` is whether there
/// is any progress at all.
///
/// "Playing..."/"Resume" for the item currently in the player keeps
/// priority over resume-target labelling -- while this exact item is
/// loaded, naming a different episode would be a lie.
pub(crate) fn primary_play_label(
    playing_this_item: bool,
    paused: bool,
    resume_episode: Option<&str>,
    has_resume: bool,
) -> String {
    if playing_this_item {
        return if paused { "Resume" } else { "Playing..." }.to_string();
    }
    match (has_resume, resume_episode) {
        (true, Some(ep)) => format!("Resume {ep}"),
        (true, None) => "Resume".to_string(),
        (false, _) => "Play".to_string(),
    }
}

/// Where the Detail page's primary action points when there is watch
/// progress. On a Movie/Episode page that is the page's own item with no
/// episode tag; on a Series page it is the in-progress episode
/// (`DetailState::next_episode` with a non-zero position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeTarget {
    pub item_id: String,
    pub item_name: String,
    /// `"S2 E6"`, when both numbers are known. `None` on a Movie/Episode
    /// page, or when the resume episode is missing its season/episode
    /// numbers (the label degrades to plain "Resume" rather than inventing
    /// one).
    pub episode_label: Option<String>,
}

impl ResumeTarget {
    /// The Movie/Episode page shape: resume this page's own item.
    fn own_item(item_id: String, item_name: String) -> Self {
        ResumeTarget {
            item_id,
            item_name,
            episode_label: None,
        }
    }

    /// The Series page shape: resume a specific episode. `None` when that
    /// episode has no watch progress -- "up next" is not "resume".
    fn from_series_episode(ep: &CardRow) -> Option<Self> {
        if ep.position_ticks <= 0 {
            return None;
        }
        Some(ResumeTarget {
            item_id: ep.id.clone(),
            item_name: ep.name.clone(),
            episode_label: match (ep.parent_index_number, ep.index_number) {
                (Some(s), Some(e)) => Some(format!("S{s} E{e}")),
                _ => None,
            },
        })
    }
}

/// §2.5's backdrop fallback chain: own `BackdropImageTags[0]` first, else
/// `ParentBackdropImageTags[0]` (resolved against `ParentBackdropItemId`,
/// not this item's own id) -- both fields already live on the full DTO
/// `Mirror::item()` returns, no extra fetch needed. A Season/Episode with no
/// backdrop of its own (the common case) needs this fallback to inherit the
/// series' backdrop rather than showing the flat placeholder.
pub(crate) fn backdrop_source(dto: &BaseItemDto) -> Option<(String, String)> {
    let own_id = dto.id.map(|u| u.to_string())?;
    if let Some(tag) = dto.backdrop_image_tags.first() {
        return Some((own_id, tag.clone()));
    }
    let parent_tag = dto.parent_backdrop_image_tags.first()?;
    let parent_id = dto
        .parent_backdrop_item_id
        .map(|u| u.to_string())
        .unwrap_or(own_id);
    Some((parent_id, parent_tag.clone()))
}

/// §1's full backdrop scrim (`backdrop::layer` -- see its doc comment for
/// the (a)-(d) stack). Used by both the movie/series page (`render`) and
/// the Episode page (`render_episode`).
fn backdrop_element(
    dto: &BaseItemDto,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
) -> AnyElement {
    backdrop_element_from_source(backdrop_source(dto), store, root, cx)
}

/// §6: the Episode page's own backdrop must be the *series'* backdrop, not
/// the episode's own still (or the season's, via the generic
/// `backdrop_source` fallback) -- the 16:9 hero still is meant to be the
/// only episode-specific image above the fold. Falls back to the generic
/// `backdrop_source` chain off the episode's own dto if the series can't be
/// resolved yet (offline, or not synced).
fn episode_backdrop_source(dto: &BaseItemDto, mirror: &Mirror) -> Option<(String, String)> {
    if let Some(series_id) = dto.series_id.map(|u| u.to_string()) {
        if let Some(series_dto) = mirror.item(&series_id) {
            if let Some(tag) = series_dto.backdrop_image_tags.first() {
                return Some((series_id, tag.clone()));
            }
        }
    }
    backdrop_source(dto)
}

fn backdrop_element_from_source(
    source: Option<(String, String)>,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
) -> AnyElement {
    let sharp = source.clone().and_then(|(id, tag)| {
        store.get(
            &id,
            ImageKind::Backdrop,
            &tag,
            BACKDROP_WIDTH,
            root.clone(),
            cx,
        )
    });
    let blurred = source.and_then(|(id, tag)| {
        store.get_scrim(&id, ImageKind::Backdrop, &tag, BACKDROP_WIDTH, root, cx)
    });
    if sharp.is_none() && blurred.is_none() {
        return div()
            .absolute()
            .inset_0()
            .bg(rgb(theme::SURFACE_BASE))
            .into_any_element();
    }
    crate::backdrop::layer(sharp, blurred)
}

/// §5's section labels ("Cast", "Similar Titles", "Details") -- Section
/// role (20px/Semibold), matching `home.rs::render_shelf`'s shelf-title
/// treatment.
fn section_header(label: &str) -> AnyElement {
    div()
        .text_color(rgba(theme::TEXT_PRIMARY))
        .text_xl()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(SharedString::from(label.to_string()))
        .into_any_element()
}

const CAST_PORTRAIT_SIZE: gpui::Pixels = px(72.);

/// §5's cast row: circular headshots, horizontal scroll, `dto.people`
/// entries carrying a `PrimaryImageTag` -- a cast member with no portrait is
/// skipped entirely (no empty gray circle). Portraits fetch through the
/// same `ImageStore::get` any other art does, addressed by the *person's
/// own* id -- the server's `/Items/{id}/Images/{type}` endpoint doesn't
/// distinguish a person id from an item id.
fn cast_row(
    dto: &BaseItemDto,
    store: &ImageStore,
    root: WeakEntity<Root>,
    scroll: &ScrollHandle,
    axis_lock: Rc<RefCell<AxisLock>>,
    cx: &mut App,
) -> Option<AnyElement> {
    let members: Vec<AnyElement> = dto
        .people
        .iter()
        .filter_map(|p| {
            let id = p.id?.to_string();
            let tag = p.primary_image_tag.clone()?;
            let name = p.name.clone().unwrap_or_default();
            let role = p.role.clone();
            let texture = store.get(
                &id,
                ImageKind::Primary,
                &tag,
                PORTRAIT_WIDTH,
                root.clone(),
                cx,
            );
            let portrait = match texture {
                Some(tex) => img(tex)
                    .object_fit(ObjectFit::Cover)
                    .size_full()
                    .into_any_element(),
                None => div()
                    .size_full()
                    .bg(rgb(theme::SURFACE_PANEL))
                    .into_any_element(),
            };
            Some(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .w(px(84.))
                    .flex_shrink_0()
                    .child(
                        div()
                            .size(CAST_PORTRAIT_SIZE)
                            .rounded_full()
                            .overflow_hidden()
                            .bg(rgb(theme::SURFACE_PANEL))
                            .child(portrait),
                    )
                    .child(
                        clamped_line(name, px(16.))
                            .text_xs()
                            .text_color(rgba(theme::TEXT_SECONDARY))
                            .text_center()
                            .w_full(),
                    )
                    .children(role.map(|r| {
                        clamped_line(r, px(16.))
                            .text_size(theme::TEXT_CAPTION)
                            .text_color(rgba(theme::TEXT_TERTIARY))
                            .text_center()
                            .w_full()
                    }))
                    .into_any_element(),
            )
        })
        .collect();

    if members.is_empty() {
        return None;
    }
    Some(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_header("Cast"))
            .child(edge_faded_strip(
                // Same per-rail axis lock the season strip and the similar
                // rail get -- see `axis_locked_rail`.
                axis_locked_rail(
                    div()
                        .id("detail-cast-row")
                        .flex()
                        .flex_row()
                        .w_full()
                        .gap_3()
                        .overflow_x_scroll()
                        .track_scroll(scroll)
                        .children(members),
                    scroll,
                    axis_lock,
                ),
                scroll,
            ))
            .into_any_element(),
    )
}

/// §5's "Similar titles row": poster cards reusing `cards::poster_card`,
/// horizontal scroll -- same cell component the Library grid/Home shelves
/// use. Sourced from `state.similar`; `None` when empty so the caller's
/// `.children(...)` renders nothing rather than an empty labeled section.
fn similar_row(
    state: &DetailState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
) -> Option<AnyElement> {
    if state.similar.is_empty() {
        return None;
    }
    let cell_w = px(CELL_WIDTH);
    let cards: Vec<AnyElement> = state
        .similar
        .iter()
        .map(|item| {
            let item_id = item.id.clone();
            let root_click = root.clone();
            poster_card(
                item,
                false,
                false,
                cell_w,
                store,
                root.clone(),
                cx,
                true,
                move |cx| {
                    let _ = root_click.update(cx, |root, cx| root.open_detail(item_id.clone(), cx));
                },
                None::<fn(&mut App)>,
                None::<fn(&mut App)>,
            )
        })
        .collect();
    Some(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_header("Similar Titles"))
            .child(edge_faded_strip(
                // See `axis_locked_rail`.
                axis_locked_rail(
                    div()
                        .id("detail-similar-row")
                        .flex()
                        .flex_row()
                        .w_full()
                        .gap_4()
                        .overflow_x_scroll()
                        .track_scroll(&state.similar_scroll)
                        .children(cards),
                    &state.similar_scroll,
                    state.axis_locks.similar.clone(),
                ),
                &state.similar_scroll,
            ))
            .into_any_element(),
    )
}

/// Details block: Director, Writers, Studio, Release date,
/// Added date. `Director`/`Writer` come off `dto.people` filtered by
/// `PersonKind` (`BaseItemPerson::role` is the free-text character name
/// instead); `Studio` off `dto.studios`. `None` when nothing has a value.
///
/// Split from the element build (`details_block`) because the
/// imbalance-aware hero layout must know row count before choosing which
/// column it renders in -- see `HeroColumnMetrics`.
fn details_rows(dto: &BaseItemDto) -> Vec<(&'static str, String)> {
    let people_by_kind = |kind: PersonKind| -> Option<String> {
        let names: Vec<String> = dto
            .people
            .iter()
            .filter(|p| p.type_ == Some(kind))
            .filter_map(|p| p.name.clone())
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    };
    let director = people_by_kind(PersonKind::Director);
    let writers = people_by_kind(PersonKind::Writer);
    let studio = {
        let names: Vec<String> = dto.studios.iter().filter_map(|s| s.name.clone()).collect();
        (!names.is_empty()).then(|| names.join(", "))
    };
    let release = dto
        .premiere_date
        .map(|d| d.format("%b %-d, %Y").to_string());
    let added = dto.date_created.map(|d| d.format("%b %-d, %Y").to_string());

    [
        ("Director", director),
        ("Writers", writers),
        ("Studio", studio),
        ("Release date", release),
        ("Added", added),
    ]
    .into_iter()
    .filter_map(|(label, value)| value.map(|v| (label, v)))
    .collect()
}

/// The synopsis paragraph and the Details rows form **one**
/// content block, so they move as a unit -- either the whole content column
/// sits beside the media block or the whole thing reflows full width
/// beneath it, and the reflow heuristic
/// (`HeroColumnMetrics::content_block_height`) balances one block instead
/// of two.
///
/// The internal `gap_3` matches `COLUMN_GAP`, the same constant the reflow
/// estimate prices the block with -- the two must not drift. `None` when
/// there is neither an overview nor a single Details row.
///
/// `section_gap`: space between the synopsis and the Details grid. The
/// movie/series column passes `COLUMN_GAP` (12); the episode page passes
/// `SECTION_GAP` (48).
fn content_block(
    overview: &str,
    rows: Vec<(&'static str, String)>,
    section_gap: f32,
) -> Option<AnyElement> {
    if overview.is_empty() && rows.is_empty() {
        return None;
    }
    let mut block = div().flex().flex_col().gap(px(section_gap));
    if !overview.is_empty() {
        block = block.child(
            // §5: synopsis max-width 68ch -- see `synopsis_max_width`. The
            // cap is on the paragraph itself, not on the merged block, so
            // the Details rows below still lay out against the full column.
            div()
                .text_size(theme::TEXT_BODY)
                .text_color(rgba(theme::TEXT_SECONDARY))
                .max_w(synopsis_max_width())
                .child(SharedString::from(overview.to_string())),
        );
    }
    if let Some(details) = details_block(rows) {
        block = block.child(details);
    }
    Some(block.into_any_element())
}

/// The details line's type size -- Martian Mono, top of the brand's 10-11px
/// mono band (the spec strip sits at 10; this line carries prose-length
/// values like a director's name, so it takes the larger step).
const DETAILS_LINE_TEXT: f32 = 11.0;
/// Its line height, and the per-line figure the movie/series A/B estimate
/// prices the block with.
const DETAILS_LINE_HEIGHT: f32 = 16.0;

/// One detail row as a compact mono token for the single-line rendering:
/// people get a terse prefix, dates read bare or as `ADDED <date>`, and
/// everything is uppercased (e.g. `DIR. MARK CENDROWSKI │ NOV 14, 2013 │
/// ADDED AUG 13, 2020`).
fn details_token(label: &str, value: &str) -> String {
    let token = match label {
        "Director" => format!("DIR. {value}"),
        "Writers" => format!("WR. {value}"),
        "Added" => format!("ADDED {value}"),
        // Studio and Release date carry themselves.
        _ => value.to_string(),
    };
    token.to_uppercase()
}

/// The details, as ONE Martian Mono line. Same separator language as the
/// spec strip (` │ `, bound to the cell it introduces so a wrap can never
/// strand one), all `GRIGIO` -- a quiet factual footer, not a second spec
/// strip, so no emphasis tiers or pill chrome. `flex_wrap` lets a long
/// writers list wrap on a narrow window rather than hide.
fn details_block(rows: Vec<(&'static str, String)>) -> Option<AnyElement> {
    if rows.is_empty() {
        return None;
    }
    let mut line = theme::apply_tabular_nums(
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .font_family(theme::FONT_MONO)
            .text_size(px(DETAILS_LINE_TEXT))
            .line_height(px(DETAILS_LINE_HEIGHT))
            .text_color(rgb(theme::GRIGIO)),
    );
    for (ix, (label, value)) in rows.iter().enumerate() {
        line = line.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                .when(ix > 0, |d| d.child(spec_strip::spec_separator()))
                .child(SharedString::from(details_token(label, value))),
        );
    }
    Some(line.into_any_element())
}

/// The spec strip: directly beneath the year/runtime/rating metadata line,
/// above the primary action, on both the movie/series page and the episode
/// page. The file path lives in the breakdown popover this strip's ⓘ
/// affordance opens.
///
/// `None` when there is nothing to say (a Series DTO carries no
/// `MediaStreams`/`MediaSources` of its own), so a Series page has no strip.
/// Partial data renders partially and fills in as enrichment lands; see
/// `ui::spec_strip`'s own doc comment on not regressing `merge_enrichment`.
fn spec_strip_block(
    dto: &BaseItemDto,
    open: bool,
    // The hero text column's width, so the strip can drop low-priority
    // fields (SIZE -> CONTAINER -> BITRATE) instead of wrapping awkwardly.
    column_width: f32,
    root: WeakEntity<Root>,
) -> Option<impl IntoElement> {
    let facts = MediaFacts::from_dto(dto);
    let fields = facts.fields();
    if fields.is_empty() {
        return None;
    }
    let sections = facts.breakdown();
    // INFO is a control, not a spec value, so it renders as a sibling of
    // the LAST pill on that pill's own row -- see `spec_info_button`.
    // `trailing_info` reserves the control's width on the last row so the
    // pair never orphans.
    let mut pill_rows = spec_strip::spec_strip_pill_rows(&fields, column_width, true);
    let last_pill = pill_rows.pop();
    let mut strip = div().flex().flex_col().gap(px(spec_strip::PILL_ROW_GAP));
    for pill in pill_rows {
        strip = strip.child(pill);
    }
    let strip = strip.children(last_pill.map(|pill| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(spec_strip::INFO_GAP))
            .child(pill)
            .child(spec_info_button("detail-spec-info", open).on_click(
                move |_event, _window, cx| {
                    // Idempotent setter computed from this render's `open`;
                    // see `Root::set_detail_media_info`'s doc comment.
                    let _ = root.update(cx, |root, cx| root.set_detail_media_info(!open, cx));
                },
            ))
    }));
    Some(popover_trigger(
        "detail-spec-strip",
        strip,
        open && !sections.is_empty(),
        // The strip sits mid-column with plenty of room below it, so the
        // panel hangs down from the strip's own left edge (Part B §9's
        // per-instance anchor table shape).
        gpui::Corner::TopLeft,
        gpui::point(px(0.), px(8.)),
        // 420px is `popover_panel`'s `max_h` (its width is the shared
        // 200..360 cap): a full breakdown of a many-track file runs long,
        // and the panel scrolls internally past this rather than growing.
        move || {
            popover_panel(
                "detail-spec-info-panel",
                px(420.),
                media_breakdown(&sections),
            )
        },
    ))
}

/// The spec strip's cell count and total value length, for the layout
/// estimate. Derived from the same `MediaFacts::fields()` call
/// `spec_strip_block` renders from, so the estimate can never describe a
/// different strip than the one that paints.
fn spec_metrics(dto: &BaseItemDto) -> (usize, usize) {
    let fields = MediaFacts::from_dto(dto).fields();
    let chars = fields.iter().map(|f| f.value.chars().count()).sum();
    (fields.len(), chars)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    state: &DetailState,
    mirror: &Mirror,
    store: &ImageStore,
    root: WeakEntity<Root>,
    playing_this_item: bool,
    paused: bool,
    content_width: gpui::Pixels,
    // A one-row season must not let the page collapse shorter than the
    // viewport (which would force `#detail-scroll`'s offset to clamp to 0
    // -- see `detail.rs::select_season`'s doc comment) just because there's
    // nothing below the grid to fill the space; `render_seasons_children`
    // reserves a trailing spacer sized off this.
    viewport_height: gpui::Pixels,
    offline: bool,
    cx: &mut Context<Root>,
) -> impl IntoElement {
    let Some(dto) = state.dto.clone() else {
        return div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(theme::SURFACE_BASE))
            .text_color(rgba(theme::TEXT_PRIMARY))
            .child("This item is no longer on the server.")
            .into_any_element();
    };

    // An Episode gets its own dedicated layout (docs/DESIGN-PLAYER-NAV.md
    // Part 2): 16:9 hero, series/season breadcrumb, sibling-episode rail.
    if state.is_episode {
        return render_episode(
            state,
            &dto,
            mirror,
            store,
            root,
            playing_this_item,
            paused,
            content_width,
            offline,
            cx,
        );
    }

    let backdrop = backdrop_element(&dto, store, root.clone(), cx);

    let poster_tag = dto.image_tags.get("Primary").cloned();
    let poster = if let Some(tag) = poster_tag {
        let id = dto.id.map(|u| u.to_string()).unwrap_or_default();
        store
            .get(
                &id,
                ImageKind::Primary,
                &tag,
                POSTER_WIDTH,
                root.clone(),
                cx,
            )
            .map(|tex| {
                img(tex)
                    .object_fit(ObjectFit::Cover)
                    .size_full()
                    .into_any_element()
            })
    } else {
        None
    }
    .unwrap_or_else(|| {
        div()
            .size_full()
            .bg(rgb(theme::SURFACE_PANEL))
            .into_any_element()
    });

    // §12: display-time wrapping-quote trim -- see `cards::display_title`.
    let title = display_title(&dto.name.clone().unwrap_or_default());
    let year = dto.production_year.map(|y| y.to_string());
    let runtime = dto.run_time_ticks.map(format_runtime);
    let rating = dto.official_rating.clone();
    let genres = (!dto.genres.is_empty()).then(|| dto.genres.join(", "));
    let overview = dto.overview.clone().unwrap_or_default();

    // The page's resume target. A Movie resumes its own item; a
    // Series resumes `DetailState::next_episode`'s in-progress episode. A
    // Series' own top-level `PlaybackPositionTicks` is not a resume point
    // for anything playable, so it's deliberately not consulted here.
    let resume = if state.is_series {
        state
            .next_episode
            .as_ref()
            .and_then(ResumeTarget::from_series_episode)
    } else {
        dto.user_data
            .as_ref()
            .and_then(|u| u.playback_position_ticks)
            .filter(|t| *t > 0)
            .map(|_| ResumeTarget::own_item(state.item_id.clone(), title.clone()))
    };
    let play_focused = state.area == DetailArea::Play;
    // The DTO carries `LocationType`/`PremiereDate` regardless of item
    // type, so this generic layout gets the same virtual-item guard as
    // Episode for free.
    let virtual_reason = (dto.location_type == Some(LocationType::Virtual)).then(|| {
        crate::cards::virtual_status_label(dto.premiere_date.map(|d| d.to_rfc3339()).as_deref())
    });
    let play_button = play_button_element(
        state.item_id.clone(),
        title.clone(),
        resume,
        playing_this_item,
        paused,
        play_focused,
        root.clone(),
        offline,
        virtual_reason,
    );

    // §5: "Year · Runtime · Rating · Genres" -- ONE delimited line, tabular
    // numerals. A Series gets "YearRange · N seasons · Rating" instead: its
    // `RunTimeTicks` is inherited from whichever episode was queried last,
    // not a meaningful series-wide figure. Season count off `state.seasons`
    // (already loaded) rather than `dto.child_count` (reused by several
    // item types).
    let meta_line = if state.is_series {
        let year_range = match (dto.production_year, dto.end_date, dto.status.as_deref()) {
            (Some(start), Some(end), _) => format!("{start}–{}", end.format("%Y")),
            (Some(start), None, Some("Continuing")) => format!("{start}–"),
            (Some(start), None, _) => start.to_string(),
            (None, _, _) => String::new(),
        };
        let seasons_label = (!state.seasons.is_empty()).then(|| {
            let n = state.seasons.len();
            format!("{n} season{}", if n == 1 { "" } else { "s" })
        });
        [
            (!year_range.is_empty()).then_some(year_range),
            seasons_label,
            rating,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  ·  ")
    } else {
        [year, runtime, rating, genres]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("  ·  ")
    };

    let detail_rows = details_rows(&dto);
    let (spec_field_count, spec_value_chars) = spec_metrics(&dto);
    let metrics = HeroColumnMetrics {
        column_width: hero_column_width(content_width, HERO_POSTER_WIDTH),
        title_chars: title.chars().count(),
        has_breadcrumb: false,
        has_meta_line: !meta_line.is_empty(),
        spec_field_count,
        spec_value_chars,
        synopsis_chars: overview.chars().count(),
        detail_rows: detail_rows.len(),
    };
    let split = hero_column_split(&metrics, HERO_POSTER_HEIGHT);

    // The synopsis and Details rows are ONE block (`content_block`), so this
    // tail holds at most one element. Built under exactly the presence
    // rules `HeroColumnMetrics::reflow_blocks` estimates them in.
    let mut column_tail: Vec<AnyElement> = Vec::new();
    column_tail.extend(content_block(&overview, detail_rows, COLUMN_GAP));
    let below_media = column_tail.split_off(split.min(column_tail.len()));

    // The hero/poster/title block is always child 0; season tabs + one
    // episode row per remaining child (Series only) -- see
    // `DetailState::scroll_target_index`'s doc comment for why these must
    // be *direct* children of the tracked `#detail-scroll` container. The
    // optional full-width reflow block lives *inside* this same child 0.
    let hero_row = div()
        .flex()
        .flex_row()
        // The text column must hug its own content so its local scrim does
        // too -- cross-axis stretch would otherwise pull the scrim's box
        // down to the poster's full height.
        .items_start()
        .gap_6()
        .child(
            // Brand §5: poster art, 2px radius, no shadow.
            div()
                .w(px(HERO_POSTER_WIDTH))
                .h(px(HERO_POSTER_HEIGHT))
                .flex_shrink_0()
                .rounded(theme::RADIUS_POSTER)
                .overflow_hidden()
                .child(poster),
        )
        .child(
            div()
                .relative()
                .flex()
                .flex_col()
                .gap_3()
                .max_w(px(TEXT_COLUMN_MAX_WIDTH))
                // The local scrim, first child so every text block below
                // paints on top of it.
                .child(hero_text_scrim())
                .child(
                    // Display role (34px).
                    div()
                        .text_color(rgba(theme::TEXT_PRIMARY))
                        .text_size(theme::TEXT_DISPLAY)
                        .line_height(theme::TEXT_DISPLAY_LINE_HEIGHT)
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(SharedString::from(title.clone())),
                )
                .child({
                    // §5: tabular numerals so Year/Runtime don't jitter.
                    theme::apply_tabular_nums(
                        div()
                            .text_size(theme::TEXT_METADATA)
                            .text_color(rgba(theme::TEXT_TERTIARY))
                            .child(SharedString::from(meta_line)),
                    )
                })
                .children(spec_strip_block(
                    &dto,
                    state.media_info_open,
                    metrics.column_width,
                    root.clone(),
                ))
                .child(play_button)
                // Layout A only: synopsis + Details stay in this column.
                .children(column_tail),
        );

    let hero_block = div()
        .flex()
        .flex_col()
        .gap_4()
        .child(hero_row)
        .children((!below_media.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_4()
                .w_full()
                .children(below_media)
        }))
        .into_any_element();

    let season_children = if state.is_series {
        render_seasons_children(
            state,
            mirror,
            store,
            root.clone(),
            content_width,
            viewport_height,
            cx,
        )
    } else {
        Vec::new()
    };

    // §5: "Fill the empty bottom" -- cast row and similar-titles row,
    // appended after season/episode content. Safe to append past
    // `season_children` without disturbing `DetailState::scroll_target_index`'s
    // fixed prefix (0 = hero, 1 = season tabs, 2.. = episode rows) -- that fn
    // never addresses these trailing children.
    let cast = cast_row(
        &dto,
        store,
        root.clone(),
        &state.cast_scroll,
        state.axis_locks.cast.clone(),
        cx,
    );
    let similar = similar_row(state, store, root.clone(), cx);

    let content = div()
        .size_full()
        .relative()
        .child(backdrop)
        .child(hero_seam_fade())
        .child(
            div()
                .id("detail-scroll")
                .absolute()
                .inset_0()
                .overflow_y_scroll()
                .track_scroll(&state.scroll)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .p_8()
                        .pt_16()
                        .gap_4()
                        .child(hero_block)
                        .children(season_children)
                        .children(cast)
                        .children(similar),
                ),
        )
        // The breakdown popover's click-away catcher needs a full-pane
        // container, which the strip's own mid-column mount point is not
        // (`ui::popover`'s split-mount contract).
        .when(state.media_info_open, |d| {
            let root = root.clone();
            d.child(click_away_catcher("detail-spec-info-away", move |cx| {
                let _ = root.update(cx, |root, cx| root.set_detail_media_info(false, cx));
            }))
        });

    content.into_any_element()
}

/// §5's synopsis max-width: a plain fn, not a `const`, since `Pixels`
/// arithmetic (`f32::from` + multiply) isn't `const`-evaluable here. 68
/// characters at the Body role's size, 15px * 68 * 0.5 = 510px. Used for
/// both hero layouts so the cap never drifts between them.
fn synopsis_max_width() -> gpui::Pixels {
    px(f32::from(theme::TEXT_BODY) * 68.0 * 0.5)
}

// Imbalance-aware hero layout
//
// The page has two columns above the fold: a fixed-size media block and a
// text column whose height is content-dependent. Each render picks between:
//
//   A -- media | everything else.
//   B -- media | title/meta/spec-strip/actions, with the synopsis and the
//        Details block reflowed full width *beneath* the media block.
//
// The two reflowable blocks move below the media **as a suffix**: "keep
// both" is A, "keep neither" is B, "keep the synopsis, reflow Details" is
// the in-between. Reading order is preserved.
//
// **Estimate, not measurement**: gpui 0.2.2 exposes no measure-then-layout
// hook (ARCHITECTURE.md's "Detail episode grid width" entry records the
// same gap). Every block but the synopsis has a fixed height; the
// synopsis' follows from its character count and the 68-character measure
// it's already capped to -- the constants below are real token values, not
// guesses.
//
// **Bias**: ties go to B -- a full-width synopsis is never wrong, whereas a
// tall void beside a poster is the reported defect.

/// The acceptance target: neither column ends more than ~40px above
/// the other. Layout A wins whenever it already meets this.
const COLUMN_BALANCE_TOLERANCE: f32 = 40.0;

/// `.gap_3()` -- the text column's inter-block gap on both page types.
const COLUMN_GAP: f32 = 12.0;
/// `.gap_6()` between the media block and the text column.
const HERO_ROW_GAP: f32 = 24.0;
/// `.p_8()` -- the page's own horizontal padding, both sides.
const PAGE_PADDING_X: f32 = 64.0;
/// The text column's `max_w(px(700.))`.
const TEXT_COLUMN_MAX_WIDTH: f32 = 700.0;
/// Movie/series poster: `w(220) h(330)`. Prefixed `HERO_` since
/// `POSTER_WIDTH` is `image_store`'s fetch width, a different quantity.
const HERO_POSTER_WIDTH: f32 = 220.0;
const HERO_POSTER_HEIGHT: f32 = 330.0;

/// The page's horizontal inset, per side (`.p_8()`). Applied per child
/// rather than once on a shared wrapper, since item 1's hero band is
/// full-bleed and must escape it -- see `render_episode`.
const PAGE_INSET: f32 = PAGE_PADDING_X / 2.0;
/// The flat gap between the Details section and the Season section, owned
/// in one place (`render_episode`'s hero block).
const SECTION_GAP: f32 = 48.0;
/// The hero band's own bottom padding: ring clearance only (the Play
/// button's 2px focus ring sits 4px outside the button), never a share of
/// the section gap -- the buttons-to-synopsis gap lives in `below_band`'s
/// top padding (`SECTION_GAP` minus this clearance).
const HERO_BAND_BOTTOM_CLEARANCE: f32 = 8.0;

/// Per-block heights, each read off the token the block is actually built
/// from. Implicit gpui line heights use the same 1.4x multiple gpui's text
/// system applies when no explicit `line_height` is set.
const TITLE_LINE_HEIGHT: f32 = 44.0; // theme::TEXT_DISPLAY_LINE_HEIGHT, set explicitly.
const META_LINE_HEIGHT: f32 = 19.0; // TEXT_METADATA (13px) * 1.4, rounded.
const BREADCRUMB_HEIGHT: f32 = 20.0; // text_sm (14px) * 1.4, rounded.
/// Brand §5 turned the strip into a bordered pill: one text line at
/// `TEXT_SPEC` (10px * 1.4 leading) plus `SPACE_COMPACT` padding and a 1px
/// border, top and bottom.
const SPEC_ROW_HEIGHT: f32 = 14.0 + (4.0 + 1.0) * 2.0;
const PLAY_BUTTON_HEIGHT: f32 = 52.0; // h(44) + mt_2 (8).
const SYNOPSIS_LINE_HEIGHT: f32 = 21.0; // TEXT_BODY (15px) * 1.4, rounded.

/// "1ch ~ 0.5em for proportional Latin text" -- same ratio `synopsis_max_width` uses.
const GLYPH_ADVANCE_RATIO: f32 = 0.5;
/// Martian Mono (brand §3's spec face) is fixed-pitch at 0.70em per glyph,
/// read off the vendored TTF's `hmtx`, same as `ui::spec_strip::MONO_ADVANCE`.
const MONO_ADVANCE_RATIO: f32 = 0.70;
/// §5's ` │ ` separator: three mono columns, approximated once per cell
/// (errs toward over-estimating width -- the safe direction, vs. clipping).
const SPEC_CELL_PADDING: f32 = 3.0 * 7.0;
/// The INFO control sharing the strip's row -- sourced from `ui::spec_strip`
/// rather than restated, so the estimate cannot drift from what paints.
const SPEC_INFO_CELL_WIDTH: f32 = spec_strip::INFO_CELL_W;

/// Everything `hero_column_split` needs to know about one render's right
/// column. Plain data (no gpui types) so the decision is a pure,
/// headlessly-testable function -- see `hero_layout_tests`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeroColumnMetrics {
    /// The laid-out width the text column will actually get, already capped
    /// to `TEXT_COLUMN_MAX_WIDTH`.
    pub column_width: f32,
    pub title_chars: usize,
    /// Episode pages only: the `S3 E7 · Series Name` eyebrow.
    pub has_breadcrumb: bool,
    pub has_meta_line: bool,
    /// The spec strip wraps (`flex_wrap`), so a narrow column turns it into
    /// two rows and that has to be part of the estimate.
    pub spec_field_count: usize,
    pub spec_value_chars: usize,
    pub synopsis_chars: usize,
    pub detail_rows: usize,
}

/// Stacked block heights plus the `.gap_3()` between them.
fn stacked_height(blocks: &[f32]) -> f32 {
    if blocks.is_empty() {
        return 0.0;
    }
    blocks.iter().sum::<f32>() + COLUMN_GAP * (blocks.len() - 1) as f32
}

fn wrapped_lines(chars: usize, chars_per_line: f32) -> f32 {
    if chars == 0 {
        return 0.0;
    }
    (chars as f32 / chars_per_line.max(1.0)).ceil().max(1.0)
}

impl HeroColumnMetrics {
    /// The blocks that stay beside the media block in **both** layouts:
    /// breadcrumb, title, metadata line, spec strip, primary action, and
    /// whichever secondary action this page type has.
    ///
    /// The spec strip stays in this group rather than the reflowing one:
    /// it's pinned directly beneath the metadata line and inside the hero
    /// text column's local scrim, and reflowing a 16px row full-width would
    /// break that placement while moving little of the imbalance-causing
    /// height. Its height still participates in the estimate below.
    fn top_block_height(&self) -> f32 {
        let mut blocks: Vec<f32> = Vec::new();
        if self.has_breadcrumb {
            blocks.push(BREADCRUMB_HEIGHT);
        }
        blocks.push(self.title_height());
        if self.has_meta_line {
            blocks.push(META_LINE_HEIGHT);
        }
        if self.spec_field_count > 0 {
            blocks.push(self.spec_strip_height());
        }
        blocks.push(PLAY_BUTTON_HEIGHT);
        stacked_height(&blocks)
    }

    /// The blocks that can move full-width beneath the media, in render
    /// order -- the same order, and the same presence rules, the render
    /// functions build them in. A block absent from the page (no overview,
    /// no Details rows) is absent here, so a split index means the same
    /// thing on both sides.
    ///
    /// This is at most **one** block. The synopsis and the
    /// Details rows render as a single merged element (`content_block`), so
    /// the estimate describes them as a single fixed-height entry.
    pub(crate) fn reflow_blocks(&self) -> Vec<f32> {
        match self.content_block_height() {
            0.0 => Vec::new(),
            height => vec![height],
        }
    }

    /// The merged synopsis + Details block's height, matching
    /// `content_block`'s structure: the paragraph, the Details line, and
    /// `COLUMN_GAP` between the two only when both are present. `0.0` when
    /// the page has neither.
    pub(crate) fn content_block_height(&self) -> f32 {
        let mut parts: Vec<f32> = Vec::new();
        if self.synopsis_chars > 0 {
            parts.push(self.synopsis_height());
        }
        if self.detail_rows > 0 {
            // Details render as ONE mono line regardless of row count.
            parts.push(DETAILS_LINE_HEIGHT);
        }
        stacked_height(&parts)
    }

    /// The text column's height when it keeps the first `keep` reflowable
    /// blocks. `keep == 0` is layout B; `keep == reflow_blocks().len()` is
    /// layout A.
    pub(crate) fn column_height(&self, keep: usize) -> f32 {
        let blocks = self.reflow_blocks();
        let kept: f32 = blocks.iter().take(keep).sum();
        let kept_count = keep.min(blocks.len());
        self.top_block_height() + kept + COLUMN_GAP * kept_count as f32
    }

    fn title_height(&self) -> f32 {
        let per_line = self.column_width / (f32::from(theme::TEXT_DISPLAY) * GLYPH_ADVANCE_RATIO);
        wrapped_lines(self.title_chars, per_line) * TITLE_LINE_HEIGHT
    }

    /// Capped at `synopsis_max_width()` in *both* layouts, so its wrapped
    /// line count is comparable between A and B.
    fn synopsis_height(&self) -> f32 {
        let width = self.column_width.min(f32::from(synopsis_max_width()));
        let per_line = width / (f32::from(theme::TEXT_BODY) * GLYPH_ADVANCE_RATIO);
        wrapped_lines(self.synopsis_chars, per_line) * SYNOPSIS_LINE_HEIGHT
    }

    fn spec_strip_height(&self) -> f32 {
        let strip_width = self.spec_value_chars as f32
            * (f32::from(theme::TEXT_SPEC) * MONO_ADVANCE_RATIO)
            + self.spec_field_count as f32 * SPEC_CELL_PADDING
            + SPEC_INFO_CELL_WIDTH;
        (strip_width / self.column_width.max(1.0)).ceil().max(1.0) * SPEC_ROW_HEIGHT
    }
}

/// Keep the largest number of trailing blocks in the
/// text column that still leaves it within ~40px of the media block's
/// bottom, reflowing the rest full width; when no split achieves that, take
/// whichever ends closest, ties going to the one that reflows more.
///
/// Returns how many of `reflow_blocks()` stay in the column.
pub(crate) fn hero_column_split(metrics: &HeroColumnMetrics, media_height: f32) -> usize {
    let n = metrics.reflow_blocks().len();
    let imbalance = |keep: usize| (metrics.column_height(keep) - media_height).abs();
    // Keeping everything wins outright whenever it already meets the
    // column-balance acceptance target.
    if imbalance(n) <= COLUMN_BALANCE_TOLERANCE {
        return n;
    }
    let mut best = 0;
    let mut best_gap = imbalance(0);
    for keep in 1..=n {
        // Strictly-better only, so a tie leaves `best` at the more-reflowed
        // split (this section's documented bias).
        if imbalance(keep) < best_gap {
            best = keep;
            best_gap = imbalance(keep);
        }
    }
    best
}

/// The width the text column actually lays out at: the page's content box
/// minus its own padding, the media block, and the `.gap_6()` between them,
/// capped by the column's `max_w`.
fn hero_column_width(content_width: gpui::Pixels, media_width: f32) -> f32 {
    (f32::from(content_width) - PAGE_PADDING_X - media_width - HERO_ROW_GAP)
        .clamp(240.0, TEXT_COLUMN_MAX_WIDTH)
}

// Local scrim behind the hero text column only
//
// The global backdrop scrim was pulled back so artwork reads as artwork,
// which left the breadcrumb, metadata line, and spec strip's 60%-opacity
// mono cells rendering straight over faces. Re-darkening the whole hero is
// not the fix; this is a panel that exists only where the text is, fully
// applied over the text's box and fading to nothing within ~80px of it.
//
// **GPUI reality**: no blur primitive for a `div()` in this version, and
// `linear_gradient` takes exactly two stops, so a soft-edged panel is built
// as a 3x3 tiling of 2-stop gradients around a flat core: four edge tiles
// fading along one axis, four corner tiles along the diagonal. Tiles abut
// rather than overlap, so no corner double-darkens.

/// The core's alpha over the backdrop -- heavier than the global scrim it
/// replaces locally. At 0.72 the worst case (spec-strip baseline mono, 60%
/// white, over a blown-out highlight) still composites to roughly a 3.9:1
/// contrast ratio, the floor §4 asks for.
const HERO_SCRIM_ALPHA: u8 = 0xb8;
/// §4: reaching transparent within ~80px of the text bounding box.
const HERO_SCRIM_FADE: f32 = 80.0;
/// On the left, 80px would reach across the `.gap_6()` and darken the media
/// block's own edge, so the gap itself is the whole fade budget there.
const HERO_SCRIM_FADE_LEFT: f32 = HERO_ROW_GAP;

/// `SURFACE_BASE`, not black: the text column runs past the point where
/// `backdrop.rs`'s vertical scrim has already gone fully opaque
/// `SURFACE_BASE`, and a black panel over that would paint a visible dark
/// rectangle on flat page color.
fn hero_scrim_core() -> gpui::Rgba {
    rgba(theme::tint(theme::SURFACE_BASE, HERO_SCRIM_ALPHA))
}

/// One edge/corner tile of the 3x3 panel. `angle` is gpui's own gradient
/// convention (0 = toward the top, 90 = toward the right), and the two
/// stops are always core -> transparent along that direction.
fn hero_scrim_tile(angle: f32, core_first: bool) -> Div {
    let (from, to) = if core_first {
        (hero_scrim_core(), rgba(theme::TRANSPARENT))
    } else {
        (rgba(theme::TRANSPARENT), hero_scrim_core())
    };
    div().h_full().bg(linear_gradient(
        angle,
        linear_color_stop(from, 0.0),
        linear_color_stop(to, 1.0),
    ))
}

/// §4's panel. Absolutely positioned with negative insets, so it sizes
/// itself off its parent without measuring it. Mounted as the text
/// column's first child, so every text block paints on top of it.
fn hero_text_scrim() -> AnyElement {
    let fade = px(HERO_SCRIM_FADE);
    let fade_left = px(HERO_SCRIM_FADE_LEFT);
    // Top row: transparent at the outer corners/edge, core along its bottom.
    let top = div()
        .flex()
        .flex_row()
        .h(fade)
        .child(hero_scrim_tile(135., false).w(fade_left)) // to bottom-right
        .child(hero_scrim_tile(0., true).flex_1()) // core at bottom, clear at top
        .child(hero_scrim_tile(45., true).w(fade)); // core at bottom-left
                                                    // Middle row: the flat core, with one-axis fades either side.
    let middle = div()
        .flex()
        .flex_row()
        .flex_1()
        .child(hero_scrim_tile(90., false).w(fade_left)) // clear at left
        .child(div().h_full().flex_1().bg(hero_scrim_core()))
        .child(hero_scrim_tile(90., true).w(fade)); // core at left, clear at right
    let bottom = div()
        .flex()
        .flex_row()
        .h(fade)
        .child(hero_scrim_tile(225., true).w(fade_left)) // core at top-right
        .child(hero_scrim_tile(180., true).flex_1()) // core at top, clear at bottom
        .child(hero_scrim_tile(135., true).w(fade)); // core at top-left
    div()
        .absolute()
        .top(-fade)
        .bottom(-fade)
        .left(-fade_left)
        .right(-fade)
        .flex()
        .flex_col()
        .child(top)
        .child(middle)
        .child(bottom)
        .into_any_element()
}

// The detail hero's bottom seam

/// Fade the final ~120px of the hero into page color. Same idiom as
/// `home.rs`'s own hero-bottom fade (single 2-stop band), re-stated here
/// since the two heroes are anchored differently.
///
/// The band's bottom edge is pinned exactly to the top of `backdrop.rs`'s
/// flat-opaque segment (`VERTICAL_FLAT_FRACTION`, 45% of viewport from the
/// bottom) -- the derivative discontinuity that reads as a horizontal edge.
/// It ends in the color already painted below it and starts transparent, so
/// it can only lengthen that transition, never introduce a second seam.
const HERO_SEAM_FADE_HEIGHT: f32 = 120.0;
/// Mirrors `backdrop::VERTICAL_FLAT_FRACTION` (crate-private there).
const BACKDROP_FLAT_FRACTION: f32 = 0.45;

fn hero_seam_fade() -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom(gpui::relative(BACKDROP_FLAT_FRACTION))
        .h(px(HERO_SEAM_FADE_HEIGHT))
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgba(theme::TRANSPARENT), 0.0),
            linear_color_stop(rgb(theme::SURFACE_BASE), 1.0),
        ))
        .into_any_element()
}

// Item 1: the episode page's hero band
//
// The band is ONE image with a real scrim over it, at a fixed height, and
// nothing floats on top of it. The synopsis is not over artwork at all: it
// moves out of the band entirely, onto flat NOTTE underneath.

/// The hero text block's inset from the top of the band -- breathing room
/// over the breadcrumb, not an image showcase, since the horizontal scrim
/// on the text side is already opaque.
const HERO_TOP_PADDING: f32 = 56.0;

/// Every run of hero text lives in the left 46% of the band, where the
/// horizontal scrim is opaque or near-opaque.
const HERO_TEXT_FRACTION: f32 = 0.46;

/// The width of that text column: the left 46%, minus the band's own left
/// inset. Not floored at a minimum -- a floor would let the column run past
/// 46% on a narrow window, which is exactly what the "no photographic
/// detail behind body text" criterion forbids. Capped by the shared text
/// measure so a very wide window doesn't set the title across 900px.
pub(crate) fn hero_text_width(content_width: gpui::Pixels) -> f32 {
    (f32::from(content_width) * HERO_TEXT_FRACTION - PAGE_INSET).clamp(0.0, TEXT_COLUMN_MAX_WIDTH)
}

// The scrim, bottom to top: (a) a horizontal ramp, then (b) a vertical one
// over it. Every colour is NOTTE at an alpha, so nothing here invents a
// ninth colour.
//
// **GPUI reality**: `linear_gradient` takes exactly two stops, and (a) is a
// four-stop ramp, so it's composited from three abutting horizontal
// segments -- a flat opaque block, then one 2-stop gradient per remaining
// segment, each starting at the alpha the previous one ended on. Abutting
// rather than stacked, so no band is double-darkened. (b) is a plain 2-stop
// gradient and needs no compositing.

/// (a)'s breakpoints, as fractions of the band's width, left to right. The
/// ramp returns to fully opaque on the right so faces stay illegible under
/// the right-hand column too; there is deliberately no tint, blend, or
/// accent-hued layer anywhere in the hero stack.
const HERO_SCRIM_OPAQUE_STOP: f32 = 0.42;
const HERO_SCRIM_MID_STOP: f32 = 0.70;
const HERO_SCRIM_RIGHT_OPAQUE_STOP: f32 = 0.88;
/// (a)'s alpha at 70%, the ramp's one translucent waypoint: the image reads
/// only as a dim glow between 42% and 88%, never recognisable detail.
const HERO_SCRIM_MID_ALPHA: u8 = 0xeb;
/// (b)'s stops: 25% at the top, 75% at 62%, fully opaque NOTTE by 92% -- the
/// image must dissolve into the page before the block ends, with no border,
/// divider, or shadow standing in for the fade.
const HERO_SCRIM_TOP_ALPHA: u8 = 0x40;
const HERO_SCRIM_V_MID_STOP: f32 = 0.62;
const HERO_SCRIM_V_MID_ALPHA: u8 = 0xbf;
const HERO_SCRIM_V_OPAQUE_STOP: f32 = 0.92;

fn hero_notte(alpha: u8) -> gpui::Rgba {
    rgba(theme::tint(theme::NOTTE, alpha))
}

/// (a): opaque NOTTE 0%-42%, ramping to 92% alpha at 70%, back to opaque by
/// 88%. `90.` is gpui's "toward the right" gradient angle. Four abutting
/// segments because gpui gradients are two-stop.
fn hero_horizontal_scrim() -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .flex_row()
        .child(
            div()
                .h_full()
                .w(gpui::relative(HERO_SCRIM_OPAQUE_STOP))
                .bg(rgb(theme::NOTTE)),
        )
        .child(
            div()
                .h_full()
                .w(gpui::relative(HERO_SCRIM_MID_STOP - HERO_SCRIM_OPAQUE_STOP))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(theme::NOTTE), 0.0),
                    linear_color_stop(hero_notte(HERO_SCRIM_MID_ALPHA), 1.0),
                )),
        )
        .child(
            div()
                .h_full()
                .w(gpui::relative(
                    HERO_SCRIM_RIGHT_OPAQUE_STOP - HERO_SCRIM_MID_STOP,
                ))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(hero_notte(HERO_SCRIM_MID_ALPHA), 0.0),
                    linear_color_stop(rgb(theme::NOTTE), 1.0),
                )),
        )
        .child(div().h_full().flex_1().bg(rgb(theme::NOTTE)))
}

/// (b): 25% alpha at the top, 75% at 62%, fully opaque NOTTE from 92% down,
/// dissolving the band into the page with no seam. `180.` is gpui's "toward
/// the bottom". Three abutting segments because gpui gradients are two-stop.
fn hero_vertical_scrim() -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .child(
            div()
                .w_full()
                .h(gpui::relative(HERO_SCRIM_V_MID_STOP))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(hero_notte(HERO_SCRIM_TOP_ALPHA), 0.0),
                    linear_color_stop(hero_notte(HERO_SCRIM_V_MID_ALPHA), 1.0),
                )),
        )
        .child(
            div()
                .w_full()
                .h(gpui::relative(
                    HERO_SCRIM_V_OPAQUE_STOP - HERO_SCRIM_V_MID_STOP,
                ))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(hero_notte(HERO_SCRIM_V_MID_ALPHA), 0.0),
                    linear_color_stop(rgb(theme::NOTTE), 1.0),
                )),
        )
        .child(div().w_full().flex_1().bg(rgb(theme::NOTTE)))
}

/// The band's single image, `object-fit: cover` across the full width.
/// Unlike `backdrop::layer` (the movie/series page's four-layer stack) this
/// is deliberately just the sharp texture -- layering `backdrop::layer`'s
/// own scrims underneath the new ones would double-darken the left half.
fn hero_backdrop_image(
    source: Option<(String, String)>,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
) -> AnyElement {
    let texture = source
        .and_then(|(id, tag)| store.get(&id, ImageKind::Backdrop, &tag, BACKDROP_WIDTH, root, cx));
    match texture {
        Some(tex) => img(tex)
            .object_fit(ObjectFit::Cover)
            .size_full()
            .into_any_element(),
        // Same flat placeholder every other art slot in the app uses while
        // its fetch is in flight.
        None => div()
            .size_full()
            .bg(rgb(theme::SURFACE_RAISED))
            .into_any_element(),
    }
}

/// §6's per-tab underline bar. Height 3px, accent-colored, `w_full()` under
/// its own tab's label -- always mounted (avoiding a label-height jump when
/// selection moves) and cross-fades opacity between 0/1 over
/// `theme::tab_indicator_animation()`'s 220ms via the same "fresh element
/// id per transition" trick `cards.rs::focus_transition` uses for this
/// gpui version's lack of a reverse/interrupt animation primitive.
///
/// **Honest approximation**: a per-tab fade, not a single element sliding
/// between tabs -- a true slide needs each tab's committed x-offset, not
/// available mid-render in this gpui version (ARCHITECTURE.md's "Detail
/// episode grid width" entry notes the same gap elsewhere).
fn season_tab_underline(tab_id: SharedString, selected: bool) -> AnyElement {
    let (start_t, end_t): (f32, f32) = if selected { (0.0, 1.0) } else { (1.0, 0.0) };
    let anim_id = SharedString::from(format!("{tab_id}-underline-{selected}"));
    div()
        .mt_1()
        .w_full()
        .h(px(3.))
        .rounded_full()
        .bg(rgb(theme::ACCENT))
        .with_animation(
            anim_id,
            theme::tab_indicator_animation(),
            move |el, delta| {
                let t = start_t + (end_t - start_t) * delta;
                el.opacity(t)
            },
        )
        .into_any_element()
}

/// Returns the season-tabs row and each episode row as *separate* elements
/// -- `render` splices these in as direct children of `#detail-scroll`'s
/// tracked container, so `DetailState::scroll_target_index` can address one
/// specific row via `ScrollHandle::scroll_to_item` (which only sees a
/// tracked container's direct children, not anything nested in a wrapper).
fn render_seasons_children(
    state: &DetailState,
    mirror: &Mirror,
    store: &ImageStore,
    root: WeakEntity<Root>,
    content_width: gpui::Pixels,
    viewport_height: gpui::Pixels,
    cx: &mut Context<Root>,
) -> Vec<AnyElement> {
    let _ = mirror;
    // §6: each tab paints its own underline bar (`season_tab_underline`)
    // rather than a `surface.overlay` fill on the selected pill. The
    // keyboard-focus ring stays (ARCHITECTURE.md's documented
    // exception for the season-tab pill).
    let season_tabs = state
        .seasons
        .iter()
        .enumerate()
        .map(|(ix, season)| {
            let selected = ix == state.selected_season;
            let focused = state.area == DetailArea::Seasons && selected;
            let root = root.clone();
            let tab_id = SharedString::from(format!("season-{ix}"));
            div()
                .id(tab_id.clone())
                .flex()
                .flex_col()
                .items_center()
                // The strip is a scroll container, and a flex item's
                // default `flex-shrink: 1` would squeeze the tabs to fit
                // instead of overflowing it.
                .flex_shrink_0()
                // The focus ring is an absolutely-positioned overlay, so
                // the tab has to be a positioning context for it.
                .relative()
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(focused, |d| d.child(focus_ring(theme::RADIUS_CONTROL)))
                .text_color(if selected {
                    rgba(theme::TEXT_PRIMARY)
                } else {
                    rgba(theme::TEXT_SECONDARY)
                })
                .child(SharedString::from(season.name.clone()))
                .child(season_tab_underline(tab_id, selected))
                .on_click(move |_event, _window, cx| {
                    let _ = root.update(cx, |root, cx| root.select_season(ix, cx));
                })
        })
        .collect::<Vec<_>>();

    let columns = columns_for_width(content_width).max(1);
    let cell_w = px(CELL_WIDTH);
    let row_count = state.episodes.len().div_ceil(columns).max(1);
    let rows: Vec<AnyElement> = (0..row_count)
        .map(|row_ix| {
            let row_cards: Vec<AnyElement> = (0..columns)
                .filter_map(|col| {
                    let ix = row_ix * columns + col;
                    state.episodes.get(ix).map(|ep| {
                        let focused =
                            state.area == DetailArea::Episodes && state.episode_focus.index == ix;
                        let ep_id = ep.id.clone();
                        let ep_name = ep.name.clone();
                        let root = root.clone();
                        let click_root = root.clone();
                        let click_ep_id = ep_id.clone();
                        episode_card(
                            ep,
                            focused,
                            cell_w,
                            store,
                            root.clone(),
                            cx,
                            // Click/Return navigates to the episode's own
                            // Detail page; playback only starts from an
                            // explicit affordance.
                            move |cx| {
                                let _ = click_root.update(cx, |root, cx| {
                                    root.open_detail(click_ep_id.clone(), cx)
                                });
                            },
                            // The card's hover play-glyph overlay.
                            move |cx| {
                                let _ = root.update(cx, |root, cx| {
                                    root.play_item(ep_id.clone(), ep_name.clone(), cx)
                                });
                            },
                        )
                    })
                })
                .collect();
            div()
                .flex()
                .flex_row()
                .gap(px(CELL_GAP))
                .pb(px(CELL_GAP))
                .children(row_cards)
                .into_any_element()
        })
        .collect();

    // Keyboard selection scroll-into-view: `scroll_to_item`'s FirstVisible
    // strategy is a no-op when the tab is already visible, so calling it
    // every render while focus is on the strip costs nothing.
    if state.area == DetailArea::Seasons {
        state.season_scroll.scroll_to_item(state.selected_season);
    }

    // Element 0: season tabs. Elements 1..: one per episode row -- see
    // `scroll_target_index`'s doc comment for why these must stay separate
    // direct children. The fade wrapper keeps that shape: it *is* element
    // 0, so `SEASON_TABS_CHILD_INDEX` still addresses the same child.
    let mut children = Vec::with_capacity(2 + rows.len());
    children.push(
        edge_faded_strip(
            // The season strip is a real horizontal scroller, so it fights
            // `#detail-scroll` for a trackpad gesture -- see `axis_locked_rail`.
            axis_locked_rail(
                div()
                    .id("season-tabs")
                    .flex()
                    .flex_row()
                    .w_full()
                    .gap_2()
                    // Room for a focused tab's ring. A scroll container
                    // clips both axes to its own bounds
                    // (`Style::overflow_mask` in gpui 0.2.2 returns a mask
                    // whenever *either* axis is non-visible), so without
                    // this the ring's top and bottom edges are cut off.
                    .py(theme::FOCUS_RING_OUTSET)
                    .overflow_x_scroll()
                    .track_scroll(&state.season_scroll)
                    .children(season_tabs),
                &state.season_scroll,
                state.axis_locks.seasons.clone(),
            ),
            &state.season_scroll,
        )
        .mt_4()
        .into_any_element(),
    );
    children.extend(rows);

    // Required behavior 3 (season-switch stability): reserve enough height
    // for the grid section as a whole to fill at least one viewport,
    // regardless of how few episodes this season has. Without this, a
    // one-row season on a show whose page has little else below the grid
    // (no cast/similar/details -- see `render`'s "fill the empty bottom"
    // comment) can leave total page content shorter than the viewport,
    // forcing `#detail-scroll`'s scroll offset to clamp all the way to 0 --
    // on top of whatever `detail.rs::select_season`'s explicit
    // `scroll_to_item` re-anchor already does, since that only guarantees
    // the season-tabs row itself stays visible, not that nothing below it
    // shifts. Deliberately reserves off the *whole* viewport height (not
    // `viewport_height` minus the hero block's own, content-dependent
    // height -- the synopsis text wraps, so the hero has no fixed height to
    // subtract here) -- an honest over-reservation that trades a few dozen
    // extra px of blank space below a short season's single row for never
    // having to guess the hero's height. `row_count` (not `state.episodes.
    // len()`) so the spacer accounts for a partially-filled last row the
    // same way the grid itself does.
    let rows_height = px(row_count as f32 * EPISODE_ROW_HEIGHT);
    let spacer_height = (viewport_height - rows_height).max(px(0.));
    if spacer_height > px(0.) {
        children.push(div().h(spacer_height).w_full().into_any_element());
    }

    children
}

// Episode Detail page (docs/DESIGN-PLAYER-NAV.md Part 2)
//
// `render_episode` is a dedicated layout: uncropped 16:9 hero, clickable
// series/season breadcrumb, same-season sibling rail sharing
// `cards::episode_card` with the series page's own episode rail.

/// Shared "text + trailing chevron" link affordance. Two call sites:
/// `breadcrumb_element`'s series-name crumb, and
/// `render_episode_siblings_children`'s "All seasons" link.
/// `TEXT_SECONDARY` at rest, `ACCENT` + underline on hover (matching
/// `button()`/`list_row`'s hover convention), always-visible trailing
/// `chevron-right.svg`. Caller attaches `.on_click(...)` since every use
/// navigates to a different target.
fn nav_link_with_chevron(id: SharedString, label: SharedString) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_0p5()
        .cursor_pointer()
        .text_color(rgba(theme::TEXT_SECONDARY))
        // Brand §2: hover is not a colour event -- brighten + underline,
        // same idiom as `ui::spec_strip::spec_info_button`.
        .hover(|d| d.text_color(rgba(theme::TEXT_PRIMARY)).underline())
        .child(label)
        .child(
            svg()
                .path("icons/chevron-right.svg")
                .w(px(12.))
                .h(px(12.))
                .flex_shrink_0()
                .text_color(rgba(theme::TEXT_TERTIARY)),
        )
}

/// The `S2 E4 · Series Name` breadcrumb: both the season number and series
/// name are independently clickable, each jumping to the series' Detail
/// page pre-selected to this episode's season (`Root::open_detail_at_season`,
/// matching by the season's own `IndexNumber` so a "Specials" season
/// numbered 0 isn't misselected). Degrades to an empty element when the DTO
/// carries no `SeriesId`.
fn breadcrumb_element(dto: &BaseItemDto, root: WeakEntity<Root>) -> AnyElement {
    let Some(series_id) = dto.series_id.map(|u| u.to_string()) else {
        return div().into_any_element();
    };
    let season_number = dto.parent_index_number;
    let episode_number = dto.index_number;
    let series_name = display_title(&dto.series_name.clone().unwrap_or_default());

    let mut parts: Vec<AnyElement> = Vec::new();
    if let Some(s) = season_number {
        let root_c = root.clone();
        let sid = series_id.clone();
        parts.push(
            div()
                .id("crumb-season")
                .cursor_pointer()
                .text_color(rgba(theme::TEXT_TERTIARY))
                .hover(|d| d.text_color(rgba(theme::TEXT_PRIMARY)))
                .child(SharedString::from(format!("S{s}")))
                .on_click(move |_event, _window, cx| {
                    let _ = root_c.update(cx, |root, cx| {
                        root.open_detail_at_season(sid.clone(), Some(s), cx)
                    });
                })
                .into_any_element(),
        );
    }
    if let Some(e) = episode_number {
        parts.push(
            div()
                .text_color(rgba(theme::TEXT_TERTIARY))
                .child(SharedString::from(format!(" E{e}")))
                .into_any_element(),
        );
    }
    parts.push(
        div()
            .text_color(rgba(theme::TEXT_TERTIARY))
            .child(SharedString::from(" · "))
            .into_any_element(),
    );
    parts.push(
        nav_link_with_chevron(
            SharedString::from("crumb-series"),
            SharedString::from(series_name),
        )
        .on_click(move |_event, _window, cx| {
            let _ = root.update(cx, |root, cx| {
                root.open_detail_at_season(series_id.clone(), season_number, cx)
            });
        })
        .into_any_element(),
    );

    // Brand §5: Martian Mono `GRIGIO` breadcrumb -- the family and size are
    // set once here for every crumb.
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .font_family(theme::FONT_MONO)
        .text_size(theme::TEXT_SPEC)
        .children(parts)
        .into_any_element()
}

/// "This is the episode whose page you're on" render as `focus`:
/// `ui::components::focus_ring`, drawn by `cards::focus_art_box` for any
/// card given `focused: true`. This rail passes `focused = is_current ||
/// keyboard_focused`, so the current episode gets the ring for free.
///
/// The sibling-episode rail: every episode in `state.episodes` (the current
/// episode's siblings), sharing `cards::episode_card` with the series
/// page's own episode rail. Clicking a sibling calls
/// `Root::open_episode_in_place` -- an in-place `DetailState` swap
/// (`Nav::replace`, no history push), not `play_item`.
///
/// Same "flatten into direct children" shape as `render_seasons_children`
/// -- element 0 is the "This season" header, elements 1.. are one per
/// sibling-episode row.
fn render_episode_siblings_children(
    state: &DetailState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    content_width: gpui::Pixels,
    cx: &mut Context<Root>,
) -> Vec<AnyElement> {
    let columns = columns_for_width(content_width).max(1);
    let cell_w = px(CELL_WIDTH);
    let row_count = state.episodes.len().div_ceil(columns).max(1);
    let rows: Vec<AnyElement> = (0..row_count)
        .map(|row_ix| {
            let row_cards: Vec<AnyElement> = (0..columns)
                .filter_map(|col| {
                    let ix = row_ix * columns + col;
                    state.episodes.get(ix).map(|ep| {
                        let is_current = ep.id == state.item_id;
                        let keyboard_focused =
                            state.area == DetailArea::Episodes && state.episode_focus.index == ix;
                        let focused = is_current || keyboard_focused;
                        let ep_id = ep.id.clone();
                        let ep_name = ep.name.clone();
                        let root = root.clone();
                        let play_root = root.clone();
                        let play_ep_id = ep_id.clone();
                        let card = episode_card(
                            ep,
                            focused,
                            cell_w,
                            store,
                            root.clone(),
                            cx,
                            move |cx| {
                                let _ = root.update(cx, |root, cx| {
                                    root.open_episode_in_place(ep_id.clone(), cx)
                                });
                            },
                            // The card's hover play-glyph overlay.
                            move |cx| {
                                let _ = play_root.update(cx, |root, cx| {
                                    root.play_item(play_ep_id.clone(), ep_name.clone(), cx)
                                });
                            },
                        );
                        // No extra "current episode" wrapper: `focused` above
                        // already covers `is_current`, and `focus_art_box`
                        // draws the one shared ring for it.
                        card
                    })
                })
                .collect();
            div()
                .flex()
                .flex_row()
                .gap(px(CELL_GAP))
                .pb(px(CELL_GAP))
                // The hero is full-bleed, so the page's horizontal inset is
                // applied per child rather than on a shared wrapper.
                .px(px(PAGE_INSET))
                .children(row_cards)
                .into_any_element()
        })
        .collect();

    let mut children = Vec::with_capacity(1 + rows.len());
    children.push(sibling_rail_header(state, root));
    children.extend(rows);
    children
}

/// The sibling rail's own header row: "Season N ·  All seasons ›" when the
/// DTO carries a season number, with "All seasons" sharing
/// `breadcrumb_element`'s `nav_link_with_chevron` link
/// (`Root::open_detail_at_season`, pre-selected to this episode's season).
/// Degrades to a plain "This season" label if the DTO is missing a
/// `SeriesId`.
fn sibling_rail_header(state: &DetailState, root: WeakEntity<Root>) -> AnyElement {
    let season_number = state.dto.as_ref().and_then(|d| d.parent_index_number);
    let label = match season_number {
        Some(n) => format!("Season {n}"),
        None => "This season".to_string(),
    };
    let series_id = state
        .dto
        .as_ref()
        .and_then(|d| d.series_id)
        .map(|u| u.to_string());

    // No top margin: the gap between the Details block and this Season
    // header is a flat `SECTION_GAP`, owned by `render_episode`'s hero
    // block padding.
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .text_sm()
        .px(px(PAGE_INSET))
        .pb_4()
        .child(
            div()
                .text_color(rgba(theme::TEXT_TERTIARY))
                .child(SharedString::from(label)),
        );

    if let Some(series_id) = series_id {
        row = row
            .child(div().text_color(rgba(theme::TEXT_TERTIARY)).child(" · "))
            .child(
                nav_link_with_chevron(
                    SharedString::from("all-seasons"),
                    SharedString::from("All seasons"),
                )
                .on_click(move |_event, _window, cx| {
                    let _ = root.update(cx, |root, cx| {
                        root.open_detail_at_season(series_id.clone(), season_number, cx)
                    });
                }),
            );
    }

    row.into_any_element()
}

/// Kicks a prefetch (`ImageStore::get`, coalesced/no-op if already inflight
/// or cached) for the prev/next sibling episodes' art, so `[`/`]` and an
/// adjacent-thumbnail click paint instantly instead of a
/// blurhash-then-fade-in. Deliberately *low priority*: never calls
/// `bump_generation`/`cancel_below_priority` itself, so a real scroll/nav
/// elsewhere is still free to cancel it (docs/DATA.md §3).
///
/// Sourced entirely from `state.episodes` -- zero extra `Mirror` reads.
/// Neighbors past a season boundary are not prefetched here; a
/// season-boundary prev/next still works, it just skips the instant paint.
fn prefetch_adjacent_episode_hero(
    state: &DetailState,
    store: &ImageStore,
    root: WeakEntity<Root>,
    cx: &mut App,
) {
    let Some(current_ix) = state.episodes.iter().position(|e| e.id == state.item_id) else {
        return;
    };
    let neighbor_indices = [current_ix.checked_sub(1), current_ix.checked_add(1)];
    for ep in neighbor_indices
        .into_iter()
        .flatten()
        .filter_map(|ix| state.episodes.get(ix))
    {
        match crate::cards::rail_art_source(ep) {
            crate::cards::RailArtSource::Own(tag) => {
                let _ = store.get(
                    &ep.id,
                    ImageKind::Primary,
                    tag,
                    BACKDROP_WIDTH,
                    root.clone(),
                    cx,
                );
            }
            crate::cards::RailArtSource::ParentBackdrop(id, tag) => {
                let _ = store.get(
                    id,
                    ImageKind::Backdrop,
                    tag,
                    BACKDROP_WIDTH,
                    root.clone(),
                    cx,
                );
            }
            crate::cards::RailArtSource::None => {}
        }
    }
}

/// The Episode Detail page: a full-width hero band carrying ONE image and
/// one two-layer scrim, with every run of hero text -- breadcrumb, title,
/// meta line, spec strip, action row -- inside its scrimmed left 46%; then,
/// on flat NOTTE below it, the synopsis and Details grid, then the
/// sibling-episode rail. Does not run the movie/series page's A/B
/// column-balance estimate: there is no second column to balance, so the
/// synopsis is always below the band and never over artwork.
#[allow(clippy::too_many_arguments)]
fn render_episode(
    state: &DetailState,
    dto: &BaseItemDto,
    mirror: &Mirror,
    store: &ImageStore,
    root: WeakEntity<Root>,
    playing_this_item: bool,
    paused: bool,
    content_width: gpui::Pixels,
    offline: bool,
    cx: &mut Context<Root>,
) -> AnyElement {
    // ONE image for the whole hero band -- the series' backdrop, per
    // `episode_backdrop_source`. A per-episode thumbnail belongs in the
    // Season row below, where every sibling already has one.
    let backdrop = hero_backdrop_image(
        episode_backdrop_source(dto, mirror),
        store,
        root.clone(),
        cx,
    );
    // §12: display-time wrapping-quote trim -- see `cards::display_title`.
    let title = display_title(&dto.name.clone().unwrap_or_default());
    prefetch_adjacent_episode_hero(state, store, root.clone(), cx);
    let breadcrumb = breadcrumb_element(dto, root.clone());

    let year = dto.production_year.map(|y| y.to_string());
    let runtime = dto.run_time_ticks.map(format_runtime);
    let rating = dto.official_rating.clone();
    let genres = (!dto.genres.is_empty()).then(|| dto.genres.join(", "));
    let overview = dto.overview.clone().unwrap_or_default();
    // §5: same "Year · Runtime · Rating · Genres" one-line format as the
    // movie/series page. Unlike the Series page (`render`'s own meta_line
    // branch), an Episode's own runtime is exactly the
    // figure being described here, so no series-style branch is needed.
    let meta_line = [year, runtime, rating, genres]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  ·  ");

    // An Episode page's resume target is always its own item -- no
    // episode tag, so the label reads a plain "Resume" (naming the episode
    // the page is already about would be noise), plus the start-over control.
    let resume = dto
        .user_data
        .as_ref()
        .and_then(|u| u.playback_position_ticks)
        .filter(|t| *t > 0)
        .map(|_| ResumeTarget::own_item(state.item_id.clone(), title.clone()));
    let play_focused = state.area == DetailArea::Play;
    // An Episode Detail page reached for an unaired/missing episode must
    // not offer a working Play/Resume button.
    let virtual_reason = (dto.location_type == Some(LocationType::Virtual)).then(|| {
        crate::cards::virtual_status_label(dto.premiere_date.map(|d| d.to_rfc3339()).as_deref())
    });
    let play_button = play_button_element(
        state.item_id.clone(),
        title.clone(),
        resume,
        playing_this_item,
        paused,
        play_focused,
        root.clone(),
        offline,
        virtual_reason,
    );

    // No on-band prev/next control: the season rail below is the mouse
    // affordance, and `[`/`]` (`Root::browse_adjacent_episode`) the
    // keyboard one.
    let detail_rows = details_rows(dto);
    let text_width = hero_text_width(content_width);

    // ---- item 1: the hero band ----------------------------------------
    //
    // One image, one scrim, all the hero text inside the left 46%. No
    // second column to balance, so the synopsis and Details grid always
    // render below the band, on flat page colour.
    //
    // The band's height is its CONTENT's height -- the text block is
    // in-flow, the backdrop/scrims are absolutely positioned behind it. The
    // text is top-anchored and in-flow (like the movie layout): breadcrumb
    // sits `HERO_TOP_PADDING` from the top, the band ends `PAGE_INSET`
    // under the action row, and only a wrapping title can grow it.
    let hero_band = div()
        .relative()
        .w_full()
        .flex_shrink_0()
        // Clips the image to the band. The backdrop and scrims are
        // absolutely positioned inside it, so nothing escapes into the
        // flat-NOTTE page below.
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_BASE))
        .child(div().absolute().inset_0().child(backdrop))
        // Scrim, bottom to top: horizontal ramp, then vertical over it.
        .child(hero_horizontal_scrim())
        .child(hero_vertical_scrim())
        .child(
            // Every run of hero text: in-flow (this block is what sizes
            // the band) and width-bound to the left 46% (where the
            // horizontal scrim is opaque). `relative()` so it paints above
            // the absolutely-positioned image/scrim layers.
            div()
                .relative()
                .flex()
                .flex_col()
                .pt(px(HERO_TOP_PADDING))
                // Only ring clearance, not a section gap: the 48px between
                // the button row and the synopsis is owned by ONE place,
                // `below_band`'s top padding.
                .pb(px(HERO_BAND_BOTTOM_CLEARANCE))
                .px(px(PAGE_INSET))
                .child(
                    div()
                        .w(px(text_width))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(breadcrumb)
                        .child(
                            // Display role (34px/Bold), matching the
                            // movie/series hero title.
                            div()
                                .text_color(rgba(theme::TEXT_PRIMARY))
                                .text_size(theme::TEXT_DISPLAY)
                                .line_height(theme::TEXT_DISPLAY_LINE_HEIGHT)
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(SharedString::from(title)),
                        )
                        .child({
                            theme::apply_tabular_nums(
                                div()
                                    .text_size(theme::TEXT_METADATA)
                                    .text_color(rgba(theme::TEXT_TERTIARY))
                                    .child(SharedString::from(meta_line)),
                            )
                        })
                        .children(spec_strip_block(
                            dto,
                            state.media_info_open,
                            text_width,
                            root.clone(),
                        ))
                        .child(play_button),
                ),
        );

    // ---- below the band: flat NOTTE ------------------------------------
    //
    // The synopsis is never over artwork. Rhythm: synopsis starts
    // SECTION_GAP below the button row, Details grid SECTION_GAP below the
    // synopsis (`content_block`'s internal gap), Season header SECTION_GAP
    // below the last Details row -- three equal 48s, each owned by exactly
    // one place.
    let below_band = content_block(&overview, detail_rows, SECTION_GAP).map(|block| {
        div()
            .flex()
            .flex_col()
            .px(px(PAGE_INSET))
            .pt(px(SECTION_GAP - HERO_BAND_BOTTOM_CLEARANCE))
            .child(block)
    });

    // Child 0 is the whole hero block, child 1 the Season header, children
    // 2.. the episode rows -- see `DetailState::scroll_target_index`'s doc
    // comment for why that shape has to hold.
    let hero_block = div()
        .flex()
        .flex_col()
        .child(hero_band)
        .children(below_band)
        // The gap to the Season header: the third of the three equal
        // SECTION_GAPs, applied whether or not there was anything below
        // the band.
        .pb(px(SECTION_GAP))
        .into_any_element();

    let sibling_children = if !state.episodes.is_empty() {
        render_episode_siblings_children(state, store, root.clone(), content_width, cx)
    } else {
        Vec::new()
    };

    let content = div()
        .size_full()
        .relative()
        // The band brings its own image; nothing below it sits on artwork.
        .bg(rgb(theme::SURFACE_BASE))
        .child(
            div()
                .id("detail-scroll")
                .absolute()
                .inset_0()
                .overflow_y_scroll()
                .track_scroll(&state.scroll)
                .child(
                    // No padding of its own -- the band reaches both window
                    // edges, so `PAGE_INSET` is applied per child instead.
                    div()
                        .flex()
                        .flex_col()
                        .pb_8()
                        .child(hero_block)
                        .children(sibling_children),
                ),
        )
        .when(state.media_info_open, |d| {
            let root = root.clone();
            d.child(click_away_catcher("detail-spec-info-away", move |cx| {
                let _ = root.update(cx, |root, cx| root.set_detail_media_info(false, cx));
            }))
        });

    content.into_any_element()
}

#[cfg(test)]
mod episode_detail_tests {
    use super::*;

    /// Pins: the single-line details register -- terse prefixes for people,
    /// `ADDED` for the library date, bare tokens for the rest, all uppercased.
    #[test]
    fn details_tokens_match_the_single_line_register() {
        assert_eq!(
            details_token("Director", "Mark Cendrowski"),
            "DIR. MARK CENDROWSKI"
        );
        assert_eq!(details_token("Writers", "Chuck Lorre"), "WR. CHUCK LORRE");
        assert_eq!(details_token("Added", "Aug 13, 2020"), "ADDED AUG 13, 2020");
        assert_eq!(
            details_token("Release date", "Nov 14, 2013"),
            "NOV 14, 2013"
        );
        assert_eq!(
            details_token("Studio", "Chuck Lorre Prod."),
            "CHUCK LORRE PROD."
        );
    }

    // `BaseItemDto` has no `Default` impl and `app` doesn't depend on `uuid`
    // directly, so fixtures build through `serde_json` instead.
    #[allow(dead_code)]
    const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

    /// Pins: `move_focus`'s Play->Down transition goes straight to the
    /// sibling rail on an Episode page (no season tabs to stop at).
    #[test]
    fn play_down_on_episode_page_goes_straight_to_episodes() {
        let mut state = DetailState {
            item_id: "ep-1".to_string(),
            dto: None,
            is_series: false,
            is_episode: true,
            seasons: Vec::new(),
            selected_season: 0,
            episodes: vec![CardRow {
                id: "ep-1".to_string(),
                item_type: "Episode".to_string(),
                name: "Ep 1".to_string(),
                primary_tag: None,
                blurhash: None,
                played: false,
                position_ticks: 0,
                runtime_ticks: None,
                unplayed_count: None,
                production_year: None,
                index_number: Some(1),
                parent_index_number: Some(1),
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
            }],
            area: DetailArea::Play,
            episode_focus: GridFocus::new(1),
            next_episode: None,
            similar: Vec::new(),
            scroll: ScrollHandle::new(),
            media_info_open: false,
            season_scroll: ScrollHandle::new(),
            cast_scroll: ScrollHandle::new(),
            similar_scroll: ScrollHandle::new(),
            axis_locks: RailAxisLocks::default(),
        };
        state.move_focus(crate::focus_grid::Direction::Down);
        assert_eq!(state.area, DetailArea::Episodes);
    }

    // ---- `scroll_target_index` -----------------------------

    fn base_state(is_series: bool, is_episode: bool, episode_focus: GridFocus) -> DetailState {
        DetailState {
            item_id: "x".to_string(),
            dto: None,
            is_series,
            is_episode,
            seasons: Vec::new(),
            selected_season: 0,
            episodes: Vec::new(),
            area: DetailArea::Play,
            episode_focus,
            next_episode: None,
            similar: Vec::new(),
            scroll: ScrollHandle::new(),
            media_info_open: false,
            season_scroll: ScrollHandle::new(),
            cast_scroll: ScrollHandle::new(),
            similar_scroll: ScrollHandle::new(),
            axis_locks: RailAxisLocks::default(),
        }
    }

    #[test]
    fn scroll_target_is_top_for_play_and_seasons_areas() {
        let mut state = base_state(true, false, GridFocus::new(4));
        state.area = DetailArea::Play;
        assert_eq!(state.scroll_target_index(), 0);
        state.area = DetailArea::Seasons;
        assert_eq!(state.scroll_target_index(), 0);
    }

    #[test]
    fn scroll_target_for_series_episode_grid_offsets_by_hero_and_season_tabs() {
        // child 0 = hero block, child 1 = season tabs, child 2.. = episode rows.
        let mut state = base_state(true, false, GridFocus::new(4));
        state.area = DetailArea::Episodes;
        state.episode_focus.index = 9; // row 2 at 4 columns
        assert_eq!(state.scroll_target_index(), 2 + 2);
    }

    #[test]
    fn scroll_target_for_episode_page_sibling_rail_offsets_by_hero_and_header() {
        // child 0 = hero/breadcrumb block, child 1 = "This season" header.
        let mut state = base_state(false, true, GridFocus::new(3));
        state.area = DetailArea::Episodes;
        state.episode_focus.index = 4; // row 1 at 3 columns
        assert_eq!(state.scroll_target_index(), 2 + 1);
    }

    #[test]
    fn scroll_target_for_movie_episodes_area_degrades_to_top() {
        // Unreachable in practice, but must not panic/underflow.
        let mut state = base_state(false, false, GridFocus::new(4));
        state.area = DetailArea::Episodes;
        assert_eq!(state.scroll_target_index(), 0);
    }
}

#[cfg(test)]
mod hero_layout_tests {
    use super::*;

    /// A movie page's real geometry at a typical window; only the synopsis
    /// length varies between the cases below.
    fn movie_metrics(synopsis_chars: usize) -> HeroColumnMetrics {
        HeroColumnMetrics {
            column_width: TEXT_COLUMN_MAX_WIDTH,
            title_chars: 18,
            has_breadcrumb: false,
            has_meta_line: true,
            spec_field_count: 8,
            spec_value_chars: 48,
            synopsis_chars,
            detail_rows: 5,
        }
    }

    /// Pins: a short synopsis stays within tolerance of the poster's
    /// height, so nothing reflows.
    #[test]
    fn short_synopsis_stays_side_by_side() {
        let metrics = movie_metrics(120);
        assert_eq!(
            hero_column_split(&metrics, HERO_POSTER_HEIGHT),
            metrics.reflow_blocks().len()
        );
    }

    /// Pins: a long overview (~1600 chars) overruns the poster on its own,
    /// so every reflowable block moves full width beneath the media.
    #[test]
    fn long_synopsis_reflows_everything_below_the_media_block() {
        assert_eq!(
            hero_column_split(&movie_metrics(1600), HERO_POSTER_HEIGHT),
            0
        );
    }

    /// Pins: the synopsis and Details are one block, so there is no split
    /// that separates them -- a mid-length synopsis still fits beside the
    /// poster (`split == 1`), a longer one takes the Details rows with it
    /// (`split == 0`). "Both or neither" is the point of the merge.
    #[test]
    fn mid_length_content_block_stays_beside_the_poster_until_it_reflows_whole() {
        assert_eq!(
            hero_column_split(&movie_metrics(200), HERO_POSTER_HEIGHT),
            1
        );
        assert_eq!(
            hero_column_split(&movie_metrics(900), HERO_POSTER_HEIGHT),
            0
        );
    }

    /// Pins: the merged block's height equals the two separate blocks
    /// stacked (`content_block`'s internal gap is the same `COLUMN_GAP`).
    #[test]
    fn merged_block_height_equals_the_two_blocks_it_replaces() {
        let metrics = movie_metrics(900);
        assert_eq!(
            metrics.content_block_height(),
            metrics.synopsis_height() + COLUMN_GAP + DETAILS_LINE_HEIGHT
        );
        assert_eq!(
            metrics.reflow_blocks(),
            vec![metrics.content_block_height()]
        );
    }

    /// Pins: a page with only a synopsis or only Details rows carries no
    /// phantom gap.
    #[test]
    fn merged_block_drops_the_gap_when_only_one_half_is_present() {
        let synopsis_only = HeroColumnMetrics {
            detail_rows: 0,
            ..movie_metrics(400)
        };
        assert_eq!(
            synopsis_only.content_block_height(),
            synopsis_only.synopsis_height()
        );
        let details_only = movie_metrics(0);
        assert_eq!(details_only.content_block_height(), DETAILS_LINE_HEIGHT);
    }

    /// Pins: whatever the split, the synopsis and the Details rows are
    /// always on the same side of it.
    #[test]
    fn synopsis_and_details_never_land_on_opposite_sides_of_the_split() {
        for chars in (0..2000).step_by(37) {
            let metrics = movie_metrics(chars);
            assert!(
                metrics.reflow_blocks().len() <= 1,
                "{chars}-character synopsis split the content block in two"
            );
        }
    }

    /// Pins: the split only ever moves toward reflowing more as the
    /// synopsis grows, and the first step away from "keep everything"
    /// happens only once that overruns §3's tolerance.
    #[test]
    fn split_moves_monotonically_toward_reflow_as_the_synopsis_grows() {
        // Counted as "how many blocks reflowed", not the split index -- not
        // comparable across the sweep otherwise.
        let mut reflowed_before = 0;
        let mut left_layout_a = false;
        for chars in (34..1700).step_by(34) {
            let metrics = movie_metrics(chars);
            let all = metrics.reflow_blocks().len();
            let reflowed = all - hero_column_split(&metrics, HERO_POSTER_HEIGHT);
            assert!(
                reflowed >= reflowed_before,
                "a longer synopsis must never move content back into the column \
                 ({chars} characters)"
            );
            if !left_layout_a && reflowed > 0 {
                left_layout_a = true;
                assert!(
                    (metrics.column_height(all) - HERO_POSTER_HEIGHT).abs()
                        > COLUMN_BALANCE_TOLERANCE,
                    "must not reflow while side-by-side is still within tolerance"
                );
            }
            reflowed_before = reflowed;
        }
        assert!(
            left_layout_a,
            "a long enough synopsis must reflow something"
        );
        assert_eq!(
            reflowed_before, 1,
            "the longest synopsis reflows every block"
        );
    }

    /// Pins: with nothing to reflow, the split is trivially "keep
    /// everything".
    #[test]
    fn nothing_to_reflow_keeps_an_empty_column_tail() {
        let metrics = HeroColumnMetrics {
            synopsis_chars: 0,
            detail_rows: 0,
            ..movie_metrics(0)
        };
        assert_eq!(hero_column_split(&metrics, HERO_POSTER_HEIGHT), 0);
        assert!(metrics.reflow_blocks().is_empty());
    }

    /// Pins: the synopsis is capped at the same 68-character measure in
    /// both layouts, so a wider column must not change its estimated height.
    #[test]
    fn synopsis_height_is_measure_bound_not_column_bound() {
        let narrow = movie_metrics(900).synopsis_height();
        let wide = HeroColumnMetrics {
            column_width: TEXT_COLUMN_MAX_WIDTH * 2.0,
            ..movie_metrics(900)
        }
        .synopsis_height();
        assert_eq!(narrow, wide);
    }
}

/// Item 1's acceptance criteria, as arithmetic on the two pure geometry
/// functions the hero band is built from -- gpui 0.2.2 exposes no
/// post-layout hook to app code, so what can be tested is the geometry
/// *decision*.
#[cfg(test)]
mod hero_band_geometry_tests {
    use super::*;
    use gpui::px;

    /// Measures the real thing rather than arguing from source: mounts the
    /// REAL `content_block` inside the exact wrapper stack `render_episode`
    /// builds and reads back, through canvas bounds spies, the distance
    /// from the content block's bottom edge to the header's top edge. If
    /// `content_block`/`details_block` ever grows a stray min-height or
    /// bottom padding, this measures it rather than trusting the constants.
    mod details_to_season_gap {
        use super::*;
        use gpui::{canvas, Render, TestAppContext, VisualTestContext, Window};
        use std::cell::Cell;
        use std::rc::Rc;

        struct GapProbe {
            content_bottom: Rc<Cell<f32>>,
            header_top: Rc<Cell<f32>>,
        }

        impl Render for GapProbe {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                let content_spy = self.content_bottom.clone();
                let header_spy = self.header_top.clone();
                let block = content_block(
                    "When Sheldon bans Penny from the apartment for numerous minor \
                     infractions, she decides to retaliate.",
                    vec![
                        ("Director", "A. Director".to_string()),
                        ("Release date", "Nov 10, 2008".to_string()),
                    ],
                    SECTION_GAP,
                )
                .expect("fixture has both a synopsis and rows");
                div()
                    .flex()
                    .flex_col()
                    .w(px(1200.))
                    .child(
                        // `render_episode`'s hero-block tail, verbatim.
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .relative()
                                    .flex()
                                    .flex_col()
                                    .px(px(PAGE_INSET))
                                    .pt(px(PAGE_INSET))
                                    .child(block)
                                    .child(
                                        canvas(
                                            move |bounds, _w, _cx| {
                                                content_spy.set(f32::from(bounds.bottom()));
                                            },
                                            |_b, _t, _w, _cx| {},
                                        )
                                        .absolute()
                                        .inset_0(),
                                    ),
                            )
                            .pb(px(SECTION_GAP)),
                    )
                    .child(
                        // `sibling_rail_header`'s own vertical shape.
                        div()
                            .relative()
                            .flex()
                            .flex_row()
                            .items_center()
                            .text_sm()
                            .px(px(PAGE_INSET))
                            .pb_4()
                            .child("Season 2")
                            .child(
                                canvas(
                                    move |bounds, _w, _cx| {
                                        header_spy.set(f32::from(bounds.top()));
                                    },
                                    |_b, _t, _w, _cx| {},
                                )
                                .absolute()
                                .inset_0(),
                            ),
                    )
            }
        }

        #[gpui::test]
        fn the_details_to_season_gap_is_the_flat_section_gap(cx: &mut TestAppContext) {
            let content_bottom = Rc::new(Cell::new(-1.0));
            let header_top = Rc::new(Cell::new(-1.0));
            let window = cx.add_window(|_w, _cx| GapProbe {
                content_bottom: content_bottom.clone(),
                header_top: header_top.clone(),
            });
            let cx = VisualTestContext::from_window(window.into(), cx);
            cx.run_until_parked();
            let gap = header_top.get() - content_bottom.get();
            assert!(
                (gap - SECTION_GAP).abs() < 1.0,
                "the last Details row's box ends {gap}px above the Season header's box \
                 (content bottom {}, header top {}) -- a stale min-height or extra \
                 padding has crept in (round 3: three equal flat {SECTION_GAP}px \
                 section gaps, nothing larger)",
                content_bottom.get(),
                header_top.get()
            );
        }
    }

    /// The band is content-sized now (in-flow text over an absolute
    /// backdrop), so there is no height constant left to assert -- the one
    /// piece of fixed vertical geometry is the breadcrumb's top inset,
    /// which must stay modest: it is breathing room, not an image
    /// showcase (the scrim under it is opaque on the text side).
    #[test]
    fn the_hero_top_inset_is_breathing_room_not_a_showcase() {
        assert!(
            (0.0..=96.0).contains(&HERO_TOP_PADDING),
            "hero top inset {HERO_TOP_PADDING}px has grown past breathing room"
        );
    }

    /// Pins the acceptance criterion: at any window width, no photographic
    /// detail visible behind any run of body text. The hero text column's
    /// right edge (`PAGE_INSET + hero_text_width`) must never pass the 46%
    /// bound, where the scrim is still ~95% opaque.
    #[test]
    fn hero_text_never_reaches_past_the_scrimmed_left_band() {
        for w in [
            320., 480., 640., 800., 1024., 1280., 1440., 1920., 2560., 3840.,
        ] {
            let right_edge = PAGE_INSET + hero_text_width(px(w));
            let bound = w * HERO_TEXT_FRACTION;
            assert!(
                right_edge <= bound + f32::EPSILON,
                "at {w}px wide, hero text runs to {right_edge}px, past the \
                 {bound}px scrimmed band"
            );
        }
    }

    /// Pins: the scrim's stops stay ordered and cover the whole band, or
    /// the three composited segments would overlap or leave a gap.
    #[test]
    fn the_horizontal_scrim_segments_tile_the_band_exactly_once() {
        // The three segment widths, exactly as `hero_horizontal_scrim`
        // builds them.
        let flat = HERO_SCRIM_OPAQUE_STOP;
        let middle = HERO_SCRIM_MID_STOP - HERO_SCRIM_OPAQUE_STOP;
        let remainder = 1.0 - HERO_SCRIM_MID_STOP;
        for (name, w) in [("flat", flat), ("middle", middle), ("remainder", remainder)] {
            assert!(w > 0.0, "the {name} scrim segment has no width");
        }
        assert!((flat + middle + remainder - 1.0).abs() < f32::EPSILON);
    }

    /// Pins the ramp shape: the horizontal scrim dips to exactly one
    /// translucent waypoint (0.92 at 70%) and is fully opaque at both ends;
    /// the vertical one only gets heavier top to bottom and reaches fully
    /// opaque before the band's bottom edge (no seam).
    #[test]
    #[allow(clippy::assertions_on_constants)] // the constants ARE the subject
    fn the_scrim_ramps_run_the_right_way() {
        assert!(
            HERO_SCRIM_MID_ALPHA < 0xff,
            "the 70% waypoint is the ramp's one translucent stop"
        );
        assert!(
            HERO_SCRIM_MID_ALPHA >= 0xeb,
            "rgba(20,16,13,0.92): the waypoint must stay near-opaque -- \
             recognisable detail under the right column was the reported bug"
        );
        // The right-opaque stop must sit strictly inside the band.
        assert!(HERO_SCRIM_MID_STOP < HERO_SCRIM_RIGHT_OPAQUE_STOP);
        assert!(HERO_SCRIM_RIGHT_OPAQUE_STOP < 1.0);
        // (b), top to bottom: 0.25 -> 0.75 -> opaque by 92%.
        assert!(HERO_SCRIM_TOP_ALPHA < HERO_SCRIM_V_MID_ALPHA);
        assert!(HERO_SCRIM_V_MID_STOP < HERO_SCRIM_V_OPAQUE_STOP);
        assert!(
            HERO_SCRIM_V_OPAQUE_STOP < 1.0,
            "the vertical scrim must reach full NOTTE strictly before the \
             band's bottom edge -- an image sliver at the last row of pixels \
             is the reported hard seam"
        );
    }

    /// Pins: every scrim colour is `NOTTE` at an alpha (brand §2 allows no
    /// ninth colour or accent-hued layer in the hero stack).
    #[test]
    fn every_scrim_stop_is_notte_at_an_alpha() {
        for alpha in [
            HERO_SCRIM_MID_ALPHA,
            HERO_SCRIM_TOP_ALPHA,
            HERO_SCRIM_V_MID_ALPHA,
        ] {
            assert_eq!(theme::tint(theme::NOTTE, alpha) >> 8, theme::NOTTE);
        }
        assert_eq!(theme::NOTTE, 0x14100D, "rgba(20,16,13,..) is NOTTE");
    }
}

/// The per-rail gesture state: the axis *decision* is `scroll_axis.rs`'s
/// own tested logic; this module owns that each rail gets its own lock.
#[cfg(test)]
mod rail_axis_lock_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn every_rail_gets_its_own_lock() {
        let locks = RailAxisLocks::default();
        assert!(!Rc::ptr_eq(&locks.seasons, &locks.cast));
        assert!(!Rc::ptr_eq(&locks.seasons, &locks.similar));
        assert!(!Rc::ptr_eq(&locks.cast, &locks.similar));
    }

    /// Pins: a horizontal flick on one rail must not hand the next rail a
    /// pre-locked gesture.
    #[test]
    fn one_rails_live_gesture_does_not_lock_another() {
        let locks = RailAxisLocks::default();
        let t0 = Instant::now();
        assert_eq!(
            locks.seasons.borrow_mut().on_event(30.0, 2.0, t0),
            Axis::Horizontal
        );
        let t1 = t0 + Duration::from_millis(16);
        assert_eq!(
            locks.cast.borrow_mut().on_event(0.0, 12.0, t1),
            Axis::Vertical
        );
        // ...and the season strip's own gesture is untouched by that.
        assert_eq!(
            locks.seasons.borrow_mut().on_event(18.0, 3.0, t1),
            Axis::Horizontal
        );
    }

    /// Pins: the handle `DetailState` keeps and the one moved into a
    /// closure share the same cell, not a deep copy.
    #[test]
    fn cloning_a_lock_for_a_closure_shares_the_gesture_state() {
        let locks = RailAxisLocks::default();
        let in_closure = locks.similar.clone();
        let t0 = Instant::now();
        assert_eq!(
            in_closure.borrow_mut().on_event(20.0, 1.0, t0),
            Axis::Horizontal
        );
        let t1 = t0 + Duration::from_millis(16);
        // Noisy mid-gesture event, read back through the state's own handle.
        assert_eq!(
            locks.similar.borrow_mut().on_event(1.0, 9.0, t1),
            Axis::Horizontal
        );
    }
}

#[cfg(test)]
mod next_up_tests {
    use super::*;

    /// `CardRow` has no `Default` impl (it mirrors a query's column list).
    /// `pub(super)` so `primary_action_tests` can reuse it.
    pub(super) fn episode(id: &str, index: i32) -> CardRow {
        CardRow {
            id: id.to_string(),
            item_type: "Episode".to_string(),
            name: id.to_string(),
            primary_tag: None,
            blurhash: None,
            played: false,
            position_ticks: 0,
            runtime_ticks: None,
            unplayed_count: None,
            production_year: None,
            index_number: Some(index),
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

    fn season(id: &str, index: Option<i32>) -> CardRow {
        let mut row = episode(id, 0);
        row.item_type = "Season".to_string();
        row.index_number = index;
        row
    }

    /// Pins: for next-up purposes, Specials (season 0) come last, with
    /// regular seasons keeping their own order.
    #[test]
    fn watch_order_moves_specials_after_every_regular_season() {
        let seasons = vec![
            season("specials", Some(0)),
            season("s1", Some(1)),
            season("s2", Some(2)),
        ];
        assert_eq!(
            seasons_in_watch_order(&seasons)
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1", "s2", "specials"]
        );
    }

    /// A season with no `IndexNumber` at all is not a Specials season --
    /// it keeps its place rather than being demoted.
    #[test]
    fn watch_order_leaves_un_numbered_seasons_in_place() {
        let seasons = vec![season("unnumbered", None), season("s1", Some(1))];
        assert_eq!(
            seasons_in_watch_order(&seasons)
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["unnumbered", "s1"]
        );
    }

    /// Pins: the in-progress episode still wins outright ("Resume" beats
    /// "Up Next").
    #[test]
    fn next_up_prefers_an_in_progress_episode() {
        let mut ep1 = episode("s1e1", 1);
        ep1.played = true;
        let mut ep2 = episode("s1e2", 2);
        ep2.position_ticks = 5_000;
        let picked = find_next_episode_in(vec![vec![ep1, ep2, episode("s1e3", 3)]]);
        assert_eq!(picked.map(|e| e.id), Some("s1e2".to_string()));
    }

    /// Pins: an unaired (virtual) episode is never offered as next-up.
    #[test]
    fn next_up_skips_unaired_episodes() {
        let mut aired = episode("s1e1", 1);
        aired.played = true;
        let mut unaired = episode("s1e2", 2);
        unaired.is_virtual = true;
        let mut also_unaired = episode("s1e3", 3);
        also_unaired.is_virtual = true;
        assert!(
            find_next_episode_in(vec![vec![aired, unaired, also_unaired]]).is_none(),
            "a fully-aired-and-watched season with only unaired episodes left has no next-up"
        );
    }

    /// Pins: an unwatched special must not be offered ahead of the next
    /// regular episode.
    #[test]
    fn next_up_walks_regular_seasons_before_specials() {
        let mut s1e1 = episode("s1e1", 1);
        s1e1.played = true;
        let seasons = vec![season("specials", Some(0)), season("s1", Some(1))];
        let ordered = seasons_in_watch_order(&seasons);
        let picked = find_next_episode_in(ordered.iter().map(|s| {
            if s.id == "specials" {
                vec![episode("special-1", 1)]
            } else {
                vec![s1e1.clone(), episode("s1e2", 2)]
            }
        }));
        assert_eq!(picked.map(|e| e.id), Some("s1e2".to_string()));
    }
}

/// The Play-vs-Resume decision, in isolation from any GPUI window.
#[cfg(test)]
mod primary_action_tests {
    use super::*;

    fn ep(position_ticks: i64, season: Option<i32>, number: Option<i32>) -> CardRow {
        let mut row = next_up_tests::episode("s2e6", number.unwrap_or(6));
        row.name = "The Cooper-Nowitzki Theorem".to_string();
        row.position_ticks = position_ticks;
        row.parent_index_number = season;
        row.index_number = number;
        row
    }

    #[test]
    fn no_progress_is_play() {
        assert_eq!(primary_play_label(false, false, None, false), "Play");
    }

    /// Pins: a Movie/Episode page resumes its own item, so the label stays
    /// the bare verb.
    #[test]
    fn movie_progress_is_bare_resume() {
        let target = ResumeTarget::own_item("movie-1".into(), "Blade Runner".into());
        assert_eq!(target.episode_label, None);
        assert_eq!(
            primary_play_label(false, false, target.episode_label.as_deref(), true),
            "Resume"
        );
    }

    /// Pins: a Series page's in-progress episode is named by the button
    /// itself.
    #[test]
    fn series_progress_names_the_episode() {
        let target = ResumeTarget::from_series_episode(&ep(9_000_000, Some(2), Some(6)))
            .expect("an in-progress episode is a resume target");
        assert_eq!(target.item_id, "s2e6");
        assert_eq!(target.episode_label.as_deref(), Some("S2 E6"));
        assert_eq!(
            primary_play_label(false, false, target.episode_label.as_deref(), true),
            "Resume S2 E6"
        );
    }

    /// Pins: "up next" is not "resume" -- an unwatched next episode carries
    /// no position, so it is no resume target at all.
    #[test]
    fn unwatched_next_episode_is_not_a_resume_target() {
        assert_eq!(
            ResumeTarget::from_series_episode(&ep(0, Some(2), Some(6))),
            None
        );
    }

    /// Pins: a resume episode missing season/episode numbers degrades to
    /// the bare verb rather than inventing a tag.
    #[test]
    fn missing_episode_numbers_degrade_to_bare_resume() {
        let target = ResumeTarget::from_series_episode(&ep(9_000_000, None, None))
            .expect("progress alone makes it a resume target");
        assert_eq!(target.episode_label, None);
        assert_eq!(
            primary_play_label(false, false, target.episode_label.as_deref(), true),
            "Resume"
        );
    }

    /// Pins: the playing item's transport labels outrank resume labelling.
    #[test]
    fn the_playing_item_keeps_its_transport_labels() {
        assert_eq!(
            primary_play_label(true, false, Some("S2 E6"), true),
            "Playing..."
        );
        assert_eq!(
            primary_play_label(true, true, Some("S2 E6"), true),
            "Resume"
        );
    }
}

#[cfg(test)]
mod detail_layout_kind_tests {
    use super::*;

    /// Pins: `Movie`, `Video`, `MusicVideo`, and `Recording` all fall
    /// through to the generic Movie-shaped layout.
    #[test]
    fn movie_shaped_kinds_are_neither_series_nor_episode() {
        for kind in [
            BaseItemKind::Movie,
            BaseItemKind::Video,
            BaseItemKind::MusicVideo,
            BaseItemKind::Recording,
        ] {
            assert_eq!(
                detail_layout_kind(Some(kind)),
                (false, false),
                "{kind:?} must fall through to the generic Movie-shaped layout"
            );
        }
    }

    #[test]
    fn series_is_series_only() {
        assert_eq!(
            detail_layout_kind(Some(BaseItemKind::Series)),
            (true, false)
        );
    }

    #[test]
    fn episode_is_episode_only() {
        assert_eq!(
            detail_layout_kind(Some(BaseItemKind::Episode)),
            (false, true)
        );
    }

    /// Pins: no DTO loaded yet falls through to the generic layout, not a
    /// panic.
    #[test]
    fn unknown_type_falls_through_to_movie_shaped() {
        assert_eq!(detail_layout_kind(None), (false, false));
    }
}

// ---- Detail enrichment / similar / seasons async control flow (moved
// from root.rs) -----------------------------------------------------

impl Root {
    /// docs/UX-SPEC.md §5's Detail metadata line needs `MediaStreams`, which the
    /// mirror's bulk sync doesn't request. Skipped for Series/Season items,
    /// which never carry their own streams.
    ///
    /// Also skipped when the page already *has* the enrichment: since
    /// `apply_detail_enrichment` persists each result into the mirror
    /// (`Mirror::upsert_enriched_item`), `DetailState::load` paints a
    /// previously-visited item's streams straight off disk. This is also
    /// the loop guard: enrichment writes the mirror -> the mirror change
    /// feed refreshes the open Detail page -> nothing re-enriches, since
    /// the page's DTO now carries media sources. A later sync/delta upsert
    /// rewrites the blob without the enrichment fields, so a genuinely
    /// changed item drops back to "no media sources" and re-fetches.
    pub(crate) fn spawn_detail_enrichment(&mut self, item_id: String, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let is_series = state.detail.as_ref().map(|d| d.is_series).unwrap_or(true);
        if is_series {
            return;
        }
        let already_enriched = state
            .detail
            .as_ref()
            .and_then(|d| d.dto.as_ref())
            .map(|dto| !dto.media_sources.is_empty() && !dto.media_streams.is_empty())
            .unwrap_or(false);
        if already_enriched {
            tracing::debug!(
                item_id,
                "detail enrichment already on disk; skipping the live fetch"
            );
            return;
        }
        // `JELLYBEAM_E2E`'s "zero network calls during Series/Season/Episode
        // navigation" assertion reads this counter; it's skipped for Series
        // items above, so a pure Series -> Season -> Episode browse never
        // bumps it.
        state.item_fetch_count += 1;
        let client = state.client.clone();
        let fetch_id = item_id.clone();
        self.bridge(
            cx,
            async move {
                let dto = crate::detail::fetch_media_streams(client, fetch_id.clone()).await;
                (fetch_id, dto)
            },
            move |root, (id, dto), cx| {
                if let Some(dto) = dto {
                    root.apply_detail_enrichment(id, dto, cx);
                }
            },
        );
    }

    fn apply_detail_enrichment(
        &mut self,
        item_id: String,
        dto: BaseItemDto,
        cx: &mut Context<Self>,
    ) {
        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        // Bank the fetch on disk so the *next* visit to this item paints the
        // full spec strip immediately. Done before the id check below on
        // purpose: the data is worth keeping even if the user has already
        // navigated on.
        let mirror = state.mirror.clone();
        let persisted = dto.clone();
        self.runtime.spawn(async move {
            mirror.upsert_enriched_item(persisted).await;
        });
        if let Some(detail) = &mut state.detail {
            if detail.item_id == item_id {
                // MERGE, don't replace -- see `detail::merge_enrichment`'s
                // doc comment.
                match &mut detail.dto {
                    Some(base) => crate::detail::merge_enrichment(base, dto),
                    None => detail.dto = Some(dto),
                }
                cx.notify();
            }
        }
    }

    /// docs/UX-SPEC.md §5's "Similar titles row": `GET /Items/{itemId}/Similar`,
    /// fired alongside `spawn_detail_enrichment` whenever a Detail page
    /// loads. Skipped for an Episode -- the server's recommender is keyed
    /// off Movie/Series-shaped metadata, and the Episode Detail page is
    /// already a from-context browsing surface with no natural slot for a
    /// whole-series-unrelated row.
    pub(crate) fn spawn_similar(&mut self, item_id: String, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let is_episode = state.detail.as_ref().map(|d| d.is_episode).unwrap_or(false);
        if is_episode {
            return;
        }
        let client = state.client.clone();
        let fetch_id = item_id.clone();
        self.bridge(
            cx,
            async move {
                let items = crate::detail::fetch_similar(client, fetch_id.clone()).await;
                (fetch_id, items)
            },
            move |root, (id, items), cx| root.apply_similar_items(id, items, cx),
        );
    }

    fn apply_similar_items(
        &mut self,
        item_id: String,
        items: Vec<BaseItemDto>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(detail) = &mut state.detail {
            if detail.item_id == item_id {
                detail.similar = items
                    .iter()
                    .filter_map(crate::detail::card_row_from_dto)
                    .collect();
                cx.notify();
            }
        }
    }

    /// Switches the Detail page's selected season and its episode list --
    /// both are plain `Mirror::children()` reads, synchronous and
    /// zero-network.
    ///
    /// `#detail-scroll` is a plain GPUI scroll container:
    /// `clamp_scroll_position` clamps whatever offset was already set into
    /// `[-new_max, 0]` on the very next layout, with no attempt to keep
    /// anything the user was looking at pinned. Switching to a
    /// fewer-episode season shrinks `content_size`, silently snapping a
    /// valid scroll offset toward 0 -- a visible upward jump.
    ///
    /// The fix: explicitly re-target the season-tabs row (`#detail-scroll`'s
    /// child index 1) every time the season changes, from any call site.
    /// `ScrollHandle::scroll_to_item`'s `FirstVisible` strategy is a no-op
    /// when the target is already in the viewport -- true whenever a season
    /// tab was clickable/focusable to trigger this -- so this reads as "stay
    /// exactly where you were" for the common case, correcting the offset
    /// only for `open_detail_at_season`'s fresh-page-load pre-selection.
    ///
    /// Opens/closes the Detail page spec strip's ⓘ breakdown
    /// popover. Deliberately an **idempotent setter**, not a toggle: a
    /// click on the ⓘ affordance while the popover is open lands on *two*
    /// handlers -- `ui::popover`'s full-pane `click_away_catcher` (runs
    /// first) and the affordance's own. A toggle would have those two fire
    /// in sequence and cancel out; a setter computes its target from the
    /// *render-time* `open` value each captured, so both converge on the
    /// same result regardless of order.
    pub(crate) fn set_detail_media_info(&mut self, open: bool, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if let Some(detail) = &mut state.detail {
            detail.media_info_open = open;
        }
        cx.notify();
    }

    pub(crate) fn select_season(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let Some(detail) = &mut state.detail else {
            return;
        };
        let Some(season) = detail.seasons.get(ix).cloned() else {
            return;
        };
        detail.selected_season = ix;
        detail.episodes = state
            .mirror
            .children(&season.id, Sort::IndexNumber, 0, EPISODES_LIMIT);
        detail.episode_focus = GridFocus::new(detail.episode_focus.columns.max(1));
        detail.episode_focus.clamp(detail.episodes.len());
        detail
            .scroll
            .scroll_to_item(DetailState::SEASON_TABS_CHILD_INDEX);
        cx.notify();
    }
    /// crates/player/LATENCY.md: pre-warm the play path when a playable
    /// item's Detail page opens. A resume's file/connection are always warm
    /// (played before) while a first play pays the cold cost (server
    /// file-open, page-cache miss, connection setup -- up to ~9x locally);
    /// a ranged GET of the stream head closes that gap. Skipped while
    /// offline, for non-leaf/virtual items, and for ids already warmed this
    /// session.
    pub(crate) fn spawn_stream_warm(&mut self, item_id: String, _cx: &mut Context<Self>) {
        /// 2 MiB: covers container probe + the first GOP for every corpus
        /// shape without being a meaningful transfer on a LAN or VPN.
        const WARM_STREAM_HEAD_BYTES: u64 = 2 * 1024 * 1024;
        let runtime = self.runtime.clone();
        let Some(state) = self.main_state_mut() else {
            return;
        };
        if state.offline {
            return;
        }
        let Some(dto) = state.mirror.item(&item_id) else {
            return;
        };
        if dto.location_type == Some(LocationType::Virtual)
            || !matches!(
                dto.type_,
                Some(BaseItemKind::Movie) | Some(BaseItemKind::Episode)
            )
        {
            return;
        }
        if !state.warmed_stream_ids.insert(item_id.clone()) {
            return;
        }
        let client = state.client.clone();
        runtime.spawn(async move {
            if let Err(e) = client
                .warm_stream_head(&item_id, WARM_STREAM_HEAD_BYTES)
                .await
            {
                tracing::debug!(error = %e, item = %item_id, "stream head warm failed (ignored)");
            }
        });
    }

    /// Hover-dwell prefetch (docs/UX-SPEC.md §5 / docs/DATA.md §4: 350ms dwell warms
    /// Detail images -- here, the backdrop, the one Detail image not
    /// already fetched as a grid/shelf poster).
    pub(crate) fn prefetch_detail(&mut self, item_id: String, cx: &mut Context<Self>) {
        // A bare hover, unlike a Detail-page open, is throttled before it's
        // allowed to discard a different, already-occupied preload -- see
        // `PreloadTrigger::Hover`'s doc comment.
        self.preload_item(item_id.clone(), PreloadTrigger::Hover, cx);
        let Some(state) = self.main_state() else {
            return;
        };
        let Some(dto) = state.mirror.item(&item_id) else {
            return;
        };
        let Some(tag) = dto.backdrop_image_tags.first().cloned() else {
            return;
        };
        let image_store = state.image_store.clone();
        let root = cx.entity().downgrade();
        image_store.get(
            &item_id,
            media_cache::ImageKind::Backdrop,
            &tag,
            BACKDROP_WIDTH,
            root,
            cx,
        );
    }
}
