//! `BaseItemDto` -> mirror column extraction. This is the *only* place the
//! blob gets parsed on the write path (never on the browse/read path — see
//! docs/DATA.md §1's blob-first rule).

use jellyfin_api::models::{BaseItemDto, LocationType};

/// Column values extracted from a `BaseItemDto`, ready to bind into an
/// `items` upsert.
pub(crate) struct ItemColumns {
    pub id: String,
    pub parent_id: Option<String>,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub item_type: String,
    pub name: Option<String>,
    pub sort_name: Option<String>,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
    pub production_year: Option<i32>,
    pub premiere_date: Option<String>,
    pub runtime_ticks: Option<i64>,
    pub date_created: Option<String>,
    pub played: bool,
    pub playback_position_ticks: i64,
    pub play_count: i64,
    pub is_favorite: bool,
    pub unplayed_item_count: Option<i32>,
    pub primary_tag: Option<String>,
    pub primary_blurhash: Option<String>,
    /// Artwork fallback-chain columns for Episode/Season cards.
    pub series_primary_tag: Option<String>,
    pub parent_backdrop_item_id: Option<String>,
    pub parent_backdrop_tag: Option<String>,
    /// `UserData.LastPlayedDate`; `resume()` sorts by this rather than the
    /// local write clock (see schema.rs's `last_played_date` column).
    pub last_played_date: Option<String>,
    pub overview: Option<String>,
    /// `true` when `LocationType == "Virtual"`.
    pub is_virtual: bool,
    pub series_name: Option<String>,
}

/// The text FTS5 indexes for one item (docs/DATA.md's `search` virtual table
/// columns, in order).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SearchText {
    pub name: String,
    pub original_title: String,
    pub series_name: String,
    pub overview: String,
}

/// The `parent_id` value the mirror's `idx_items_browse` index/`children()`
/// query should use for browsing, which isn't always the DTO's raw
/// `ParentId`.
///
/// Season/Episode derive it from `series_id`/`season_id` instead of trusting
/// the raw field: matches the local browse hierarchy (Season under Series,
/// Episode under Season) regardless of server-version quirks in when
/// `ParentId` itself is populated, and degrades to the raw `ParentId` if
/// those ids are absent.
fn browse_parent_id(
    item_type: &str,
    raw_parent_id: Option<String>,
    series_id: &Option<String>,
    season_id: &Option<String>,
) -> Option<String> {
    match item_type {
        "Episode" => season_id
            .clone()
            .or_else(|| series_id.clone())
            .or(raw_parent_id),
        "Season" => series_id.clone().or(raw_parent_id),
        _ => raw_parent_id,
    }
}

/// `id` is required for an item to be indexable at all; server rows should
/// always have one, but a missing id (malformed/partial payload) means the
/// row is simply skipped rather than panicking anywhere downstream.
pub(crate) fn extract_columns(item: &BaseItemDto) -> Option<ItemColumns> {
    let id = item.id?.to_string();

    let primary_tag = item.image_tags.get("Primary").cloned();
    let primary_blurhash = primary_tag.as_ref().and_then(|tag| {
        item.image_blur_hashes
            .as_ref()
            .and_then(|hashes| hashes.primary.get(tag).cloned())
    });

    let user_data = item.user_data.as_ref();

    let item_type = item
        .type_
        .map(|t| t.to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    let series_id = item.series_id.map(|u| u.to_string());
    let season_id = item.season_id.map(|u| u.to_string());
    let parent_id = browse_parent_id(
        &item_type,
        item.parent_id.map(|u| u.to_string()),
        &series_id,
        &season_id,
    );

    Some(ItemColumns {
        id,
        parent_id,
        series_id,
        season_id,
        item_type,
        name: item.name.clone(),
        sort_name: item.sort_name.clone().or_else(|| item.name.clone()),
        index_number: item.index_number,
        parent_index_number: item.parent_index_number,
        production_year: item.production_year,
        premiere_date: item.premiere_date.map(|d| d.to_rfc3339()),
        runtime_ticks: item.run_time_ticks,
        date_created: item.date_created.map(|d| d.to_rfc3339()),
        played: user_data.and_then(|u| u.played).unwrap_or(false),
        playback_position_ticks: user_data
            .and_then(|u| u.playback_position_ticks)
            .unwrap_or(0),
        play_count: i64::from(user_data.and_then(|u| u.play_count).unwrap_or(0)),
        is_favorite: user_data.and_then(|u| u.is_favorite).unwrap_or(false),
        unplayed_item_count: user_data.and_then(|u| u.unplayed_item_count),
        primary_tag,
        primary_blurhash,
        series_primary_tag: item.series_primary_image_tag.clone(),
        parent_backdrop_item_id: item.parent_backdrop_item_id.map(|u| u.to_string()),
        parent_backdrop_tag: item.parent_backdrop_image_tags.first().cloned(),
        last_played_date: user_data
            .and_then(|u| u.last_played_date)
            .map(|d| d.to_rfc3339()),
        overview: item.overview.clone(),
        is_virtual: item.location_type == Some(LocationType::Virtual),
        series_name: item.series_name.clone(),
    })
}

pub(crate) fn search_text(item: &BaseItemDto) -> SearchText {
    SearchText {
        name: item.name.clone().unwrap_or_default(),
        original_title: item.original_title.clone().unwrap_or_default(),
        series_name: item.series_name.clone().unwrap_or_default(),
        overview: item.overview.clone().unwrap_or_default(),
    }
}

/// Grafts the *enrichment-only* fields of `fresh` onto `stored`; every other
/// field of `stored` is left alone.
///
/// The seven fields (`MediaStreams`, `MediaSources`, `Chapters`, `Trickplay`,
/// `People`, `Overview`, `Genres`) are what a per-item Detail fetch adds and
/// bulk sync never requests (docs/DATA.md's field list).
///
/// Direction is reversed from the app's `detail::merge_enrichment`: `stored`
/// wins everywhere else, because the fetch's narrow `Fields=` projection
/// omits what `extract_columns` reads, and would otherwise silently clobber
/// the mirror's derived `parent_id`.
///
/// A field is grafted only when `fresh` actually has it, so re-grafting a
/// partial response can't erase what a previous one contributed.
pub(crate) fn graft_enrichment(stored: &mut BaseItemDto, fresh: &BaseItemDto) {
    if !fresh.media_streams.is_empty() {
        stored.media_streams = fresh.media_streams.clone();
    }
    if !fresh.media_sources.is_empty() {
        stored.media_sources = fresh.media_sources.clone();
    }
    if !fresh.chapters.is_empty() {
        stored.chapters = fresh.chapters.clone();
    }
    if !fresh.trickplay.is_empty() {
        stored.trickplay = fresh.trickplay.clone();
    }
    if !fresh.people.is_empty() {
        stored.people = fresh.people.clone();
    }
    if fresh.overview.is_some() {
        stored.overview = fresh.overview.clone();
    }
    if !fresh.genres.is_empty() {
        stored.genres = fresh.genres.clone();
    }
}

/// Re-serializes the DTO for blob storage.
///
/// This is the typed `BaseItemDto`'s JSON, not the original wire bytes:
/// `JellyfinClient` only returns decoded structs, and `BaseItemDto` has no
/// catch-all field map, so server fields our model doesn't know about are
/// dropped before we ever see them.
pub(crate) fn to_dto_bytes(item: &BaseItemDto) -> Vec<u8> {
    serde_json::to_vec(item).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to serialize BaseItemDto for blob storage");
        b"{}".to_vec()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jellyfin_api::models::{
        BaseItemDto, BaseItemDtoImageBlurHashes, BaseItemKind, UserItemDataDto,
    };
    use std::collections::HashMap;

    fn base_item() -> BaseItemDto {
        BaseItemDto {
            id: Some(uuid::Uuid::parse_str("e2f5a5f1-1a0b-4b3a-9c2e-000000000001").expect("uuid")),
            name: Some("Sample Movie".to_string()),
            type_: Some(BaseItemKind::Movie),
            ..Default::default()
        }
    }

    #[test]
    fn extracts_basic_columns() {
        let item = base_item();
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(cols.id, "e2f5a5f1-1a0b-4b3a-9c2e-000000000001");
        assert_eq!(cols.item_type, "Movie");
        assert_eq!(cols.name.as_deref(), Some("Sample Movie"));
        assert_eq!(cols.sort_name.as_deref(), Some("Sample Movie"));
        assert!(!cols.played);
        assert_eq!(cols.playback_position_ticks, 0);
    }

    #[test]
    fn missing_id_yields_none() {
        let mut item = base_item();
        item.id = None;
        assert!(extract_columns(&item).is_none());
    }

    #[test]
    fn user_data_columns_extracted() {
        let mut item = base_item();
        item.user_data = Some(UserItemDataDto {
            played: Some(true),
            playback_position_ticks: Some(4200),
            play_count: Some(3),
            is_favorite: Some(true),
            unplayed_item_count: Some(2),
            key: "k".to_string(),
            item_id: None,
            last_played_date: None,
            likes: None,
            played_percentage: None,
            rating: None,
        });
        let cols = extract_columns(&item).expect("has id");
        assert!(cols.played);
        assert_eq!(cols.playback_position_ticks, 4200);
        assert_eq!(cols.play_count, 3);
        assert!(cols.is_favorite);
        assert_eq!(cols.unplayed_item_count, Some(2));
    }

    #[test]
    fn image_tags_and_blurhash_extracted() {
        let mut item = base_item();
        let mut tags = HashMap::new();
        tags.insert("Primary".to_string(), "abc123".to_string());
        item.image_tags = tags;
        let mut blur = BaseItemDtoImageBlurHashes::default();
        blur.primary
            .insert("abc123".to_string(), "L6PZfSi_.AyE".to_string());
        item.image_blur_hashes = Some(blur);

        let cols = extract_columns(&item).expect("has id");
        assert_eq!(cols.primary_tag.as_deref(), Some("abc123"));
        assert_eq!(cols.primary_blurhash.as_deref(), Some("L6PZfSi_.AyE"));
    }

    /// Pins: artwork fallback fields come straight off the DTO's own
    /// `Series*`/`Parent*` fields, no derivation (unlike `browse_parent_id`).
    #[test]
    fn artwork_fallback_columns_extracted() {
        let mut item = episode_item(Some(9), Some(8), Some(2));
        item.series_primary_image_tag = Some("series-poster-tag".to_string());
        item.parent_backdrop_item_id = Some(uuid_field(2));
        item.parent_backdrop_image_tags = vec!["season-backdrop".to_string()];

        let cols = extract_columns(&item).expect("has id");
        assert_eq!(
            cols.series_primary_tag.as_deref(),
            Some("series-poster-tag")
        );
        assert_eq!(
            cols.parent_backdrop_item_id.as_deref(),
            Some(uuid_field(2).to_string()).as_deref()
        );
        assert_eq!(cols.parent_backdrop_tag.as_deref(), Some("season-backdrop"));
    }

    #[test]
    fn artwork_fallback_columns_default_to_none() {
        let item = base_item();
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(cols.series_primary_tag, None);
        assert_eq!(cols.parent_backdrop_item_id, None);
        assert_eq!(cols.parent_backdrop_tag, None);
    }

    /// Pins: `series_name` extracts straight off `SeriesName`, same shape as
    /// the artwork fallback fields above.
    #[test]
    fn series_name_extracted() {
        let mut item = episode_item(Some(9), Some(8), Some(2));
        item.series_name = Some("The Wire".to_string());
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(cols.series_name.as_deref(), Some("The Wire"));
    }

    #[test]
    fn series_name_defaults_to_none() {
        let item = base_item();
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(cols.series_name, None);
    }

    #[test]
    fn sort_name_falls_back_to_name_when_absent() {
        let mut item = base_item();
        item.sort_name = None;
        item.name = Some("The Thing".to_string());
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(cols.sort_name.as_deref(), Some("The Thing"));
    }

    fn uuid_field(n: u8) -> uuid::Uuid {
        uuid::Uuid::parse_str(&format!("e2f5a5f1-1a0b-4b3a-9c2e-{n:012}")).expect("uuid")
    }

    /// Built with all three ids distinct so the mapping tests below can tell
    /// which id `parent_id` came from (real servers return `ParentId ==
    /// SeasonId`, see `sync::item_fields`).
    fn episode_item(raw_parent: Option<u8>, series: Option<u8>, season: Option<u8>) -> BaseItemDto {
        BaseItemDto {
            id: Some(uuid_field(1)),
            name: Some("S01E01".to_string()),
            type_: Some(BaseItemKind::Episode),
            parent_id: raw_parent.map(uuid_field),
            series_id: series.map(uuid_field),
            season_id: season.map(uuid_field),
            ..Default::default()
        }
    }

    fn season_item(raw_parent: Option<u8>, series: Option<u8>) -> BaseItemDto {
        BaseItemDto {
            id: Some(uuid_field(1)),
            name: Some("Season 1".to_string()),
            type_: Some(BaseItemKind::Season),
            parent_id: raw_parent.map(uuid_field),
            series_id: series.map(uuid_field),
            season_id: None,
            ..Default::default()
        }
    }

    /// Pins: Episode `parent_id` must come from `SeasonId`, not the raw
    /// `ParentId`, so `children(season_id)` works regardless of what the
    /// raw field points at.
    #[test]
    fn episode_parent_id_prefers_season_id_over_raw_parent_id() {
        let item = episode_item(Some(9), Some(8), Some(2));
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(
            cols.parent_id.as_deref(),
            Some(uuid_field(2).to_string()).as_deref()
        );
        assert_eq!(
            cols.season_id.as_deref(),
            Some(uuid_field(2).to_string()).as_deref()
        );
    }

    #[test]
    fn episode_parent_id_falls_back_to_series_id_when_season_id_absent() {
        let item = episode_item(Some(9), Some(8), None);
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(
            cols.parent_id.as_deref(),
            Some(uuid_field(8).to_string()).as_deref()
        );
    }

    #[test]
    fn episode_parent_id_falls_back_to_raw_parent_id_when_series_and_season_absent() {
        let item = episode_item(Some(9), None, None);
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(
            cols.parent_id.as_deref(),
            Some(uuid_field(9).to_string()).as_deref()
        );
    }

    #[test]
    fn episode_parent_id_is_none_when_nothing_is_available() {
        let item = episode_item(None, None, None);
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(cols.parent_id, None);
    }

    /// Pins: same story one level up -- a Season's `parent_id` must come
    /// from `SeriesId`, not the raw field, so `children(series_id)` returns
    /// its seasons.
    #[test]
    fn season_parent_id_prefers_series_id_over_raw_parent_id() {
        let item = season_item(Some(9), Some(3));
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(
            cols.parent_id.as_deref(),
            Some(uuid_field(3).to_string()).as_deref()
        );
        assert_eq!(
            cols.series_id.as_deref(),
            Some(uuid_field(3).to_string()).as_deref()
        );
    }

    #[test]
    fn season_parent_id_falls_back_to_raw_parent_id_when_series_id_absent() {
        let item = season_item(Some(9), None);
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(
            cols.parent_id.as_deref(),
            Some(uuid_field(9).to_string()).as_deref()
        );
    }

    /// Pins: the Season/Episode override must not leak onto other types --
    /// a Movie's `parent_id` still comes straight from the raw field.
    #[test]
    fn other_item_types_use_raw_parent_id_unchanged() {
        let mut item = base_item(); // Movie
        item.parent_id = Some(uuid_field(7));
        item.series_id = Some(uuid_field(3)); // should be ignored for a Movie
        let cols = extract_columns(&item).expect("has id");
        assert_eq!(
            cols.parent_id.as_deref(),
            Some(uuid_field(7).to_string()).as_deref()
        );
    }

    /// Pins: `fresh` contributes only the fields it actually has; `stored`
    /// wins for everything else.
    #[test]
    fn graft_enrichment_takes_only_the_enrichment_fields_it_has() {
        let mut stored = base_item();
        stored.overview = Some("Synced synopsis.".to_string());
        stored.genres = vec!["Drama".to_string()];

        let mut fresh = BaseItemDto {
            id: stored.id,
            // The narrow fetch never re-sends the row's own identity fields.
            name: None,
            media_streams: vec![Default::default()],
            ..Default::default()
        };
        fresh.people = vec![Default::default()];

        graft_enrichment(&mut stored, &fresh);

        assert_eq!(stored.media_streams.len(), 1, "grafted");
        assert_eq!(stored.people.len(), 1, "grafted");
        assert_eq!(
            stored.name.as_deref(),
            Some("Sample Movie"),
            "a field `fresh` doesn't carry must not be clobbered"
        );
        assert_eq!(
            stored.overview.as_deref(),
            Some("Synced synopsis."),
            "an absent enrichment field must not erase what's already there"
        );
        assert_eq!(stored.genres, vec!["Drama".to_string()]);
    }

    #[test]
    fn search_text_defaults_missing_fields_to_empty() {
        let item = base_item();
        let text = search_text(&item);
        assert_eq!(text.name, "Sample Movie");
        assert_eq!(text.original_title, "");
        assert_eq!(text.overview, "");
    }
}
