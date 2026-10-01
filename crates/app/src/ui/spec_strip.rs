//! The **spec strip** -- brand §5's signature component and the one shared
//! technical-metadata component for the whole app (movie/series Detail,
//! Episode Detail, the library grid's poster-hover overlay, the player OSD).
//!
//! > A gelateria labels every tub with exactly what is in it, in the same
//! > format every time, honestly. That label is the spec strip, and it is the
//! > most important component in the product. (§1)
//!
//! §5's exact spec, and what each part maps to here:
//!
//! * **A 999px pill, `SURFACE` fill, 1px `HAIRLINE` border** --
//!   [`spec_strip_fitted`]'s container. This replaced an earlier fill-less,
//!   radius-less "equipment plate" treatment; the pill is what makes the
//!   strip read as a *label affixed to* the page rather than as loose text
//!   floating on it, and it is what lets the baseline tier sit on an opaque
//!   `GRIGIO` instead of an alpha tuned for hero artwork.
//! * **Martian Mono 10-11px** -- [`theme::FONT_MONO`] at
//!   [`theme::TEXT_SPEC`] (10px; see that constant on why not 11).
//! * **Values joined by ` │ ` (U+2502 with spaces)** -- [`SEPARATOR`], a real
//!   character, replacing the painted 1px hairline divs this component used
//!   to draw between cells.
//! * **Three levels of emphasis, data-driven, never hard-coded per row** --
//!   [`SpecWeight`] + [`classify`]. Every cell in every caller gets its tier
//!   from the value itself; no call site anywhere passes a weight in.
//!
//! And §6's standing prohibition, which outranks every layout concern below:
//! **never hide a spec value to make a row look tidier.** Nothing is ever
//! dropped: a container too narrow for one pill gets several stacked pills
//! ([`partition_fields`]), each a clean §5 plate, every value present.
//! (An earlier cut dropped SIZE/CONTAINER/BITRATE to fit -- rejected in the
//! field: "responsiveness loses information".)
//!
//! ## Two API limitations, stated honestly
//!
//! * **Letter-spacing.** §3 asks for `-0.02em` on the strip. The pinned
//!   `gpui = "0.2.2"` has no tracking primitive at all -- `TextStyle` has no
//!   `letter_spacing` field and `Styled` exposes no tracking method (the
//!   same finding `theme.rs`'s type-scale section already records for the
//!   Display/Title roles). Martian Mono's own wide 0.70em advance is most of
//!   what the tracking was there to produce anyway; the tracking value
//!   itself is simply not expressible in this GPUI version, so it is skipped
//!   rather than faked.
//! * **Tabular numerals** *are* available (`theme::apply_tabular_nums`, via
//!   real OpenType `tnum`/`lnum` features) and are applied to the strip, so
//!   a bitrate/size digit swapping under enrichment doesn't reflow the row.
//!
//! ## Narrow widths
//!
//! Two layers, because GPUI 0.2.2 can neither measure text nor report where
//! a `flex_wrap` row actually broke:
//!
//! * [`spec_strip`] binds each separator to the cell it introduces inside one
//!   `flex_none` pair, so a wrap can never strand a separator at the end of
//!   a row (the reported "hairlines dangling at row ends"). Every existing
//!   caller gets this for free.
//! * [`spec_strip_fitted`] is the real fix: given the container's own width,
//!   it partitions the cells into as many rows as fit ([`partition_fields`])
//!   and renders each row as its own pill, so there is no ragged wrap and
//!   no dropped value. The width is *computed*, not measured -- legitimate
//!   only because the plate is monospace at a fixed size (see
//!   [`strip_width_px`]). Callers that know their width should prefer it;
//!   a Detail page knows its column width.
//!
//! ## Enrichment behavior (do not regress the `merge_enrichment` fix)
//!
//! Media info used to flap/vanish while a Detail page enriched. Every
//! derivation here reads whatever fields are present *right now* and omits
//! anything unknown -- there are no placeholder cells, no "unknown" strings,
//! and no all-or-nothing early return. A strip that starts as
//! `1080P │ HEVC` and grows to the full eight fields as `MediaSources`
//! lands is the intended behavior, not a bug.

use gpui::{div, prelude::*, px, rgb, rgba, Div, ElementId, SharedString, Stateful, Styled};
use jellyfin_api::models::{
    AudioSpatialFormat, BaseItemDto, MediaSourceInfo, MediaStream, MediaStreamType, VideoRange,
    VideoRangeType,
};

use crate::theme;

/// Brand §3: "Martian Mono -- spec strips, keyboard shortcuts, status
/// labels. Nothing else." Vendored and registered at startup
/// (`main.rs::register_brand_fonts`), so unlike the "Menlo" this replaces it
/// doesn't depend on what the machine happens to have installed. GPUI's font
/// system still has no fallback resolution for a bare `monospace` CSS-style
/// keyword, so a real family name is still required.
pub(crate) const MONO_FAMILY: &str = theme::FONT_MONO;

/// §5's field separator: U+2502 BOX DRAWINGS LIGHT VERTICAL, **with**
/// surrounding spaces, replacing the painted 1px divider divs this component
/// used to draw. A character rather than a rule because the strip is a
/// printed label: the separators should sit on the same baseline grid as the
/// values and recede with them, not cut across the pill as geometry.
///
/// Martian Mono has no glyph for U+2502 (checked against the vendored TTF's
/// `cmap`), so CoreText substitutes it per-glyph from the system cascade --
/// which is fine for a plain box-drawing bar, and is why [`SEPARATOR_COLS`]
/// below is an estimate rather than an exactly-known advance.
pub(crate) const SEPARATOR: &str = " │ ";
/// [`SEPARATOR`]'s width in monospace columns, for [`strip_width_px`]. Three
/// characters, one of which is substituted from a fallback face at an
/// unknown advance -- counted as a full column each, i.e. on the pessimistic
/// (over-estimating) side, which costs at most one dropped low-priority cell
/// and never an unexpected wrap.
const SEPARATOR_COLS: f32 = 3.0;

/// §5's three levels of emphasis. This *is* the information design of the
/// strip: scanning a page shows how good a file is without reading a single
/// label, because the values that matter light up on their own.
///
/// | Level | Colour | When |
/// |-------|--------|------|
/// | [`SpecWeight::Baseline`] | `GRIGIO` | ordinary -- 1080p, H264, AC3, MKV, size, bitrate |
/// | [`SpecWeight::Notable`] | `PANNA` | worth noticing -- 4K, HEVC, AV1, HDR10, Direct Play, lossless |
/// | [`SpecWeight::BestInClass`] | `PISTACCHIO` | 10-bit, Dolby Vision, Atmos, DTS-HD, TrueHD |
///
/// Assigned by [`classify`] from the value string alone, inside
/// `SpecField::new` -- §5's "data-driven, never hard-coded per row" is
/// enforced structurally: `SpecField` has no constructor that takes a
/// weight, so no caller *can* hard-code one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecWeight {
    Baseline,
    Notable,
    BestInClass,
}

impl SpecWeight {
    /// The tier's colour token. One place, so a call site never picks.
    fn color(self) -> u32 {
        match self {
            SpecWeight::Baseline => theme::TEXT_SPEC_BASELINE,
            SpecWeight::Notable => theme::TEXT_SPEC_NOTABLE,
            SpecWeight::BestInClass => theme::TEXT_SPEC_BEST,
        }
    }
}

/// Which slot a cell came from. The strip itself never *renders* this (see
/// [`SpecField`]'s "label-agnostic" note), but priority-dropping has to
/// know which cells are the expendable ones, and re-deriving that from the
/// rendered string would mean pattern-matching `"MKV"` against a container
/// list -- fragile, and wrong the first time a codec and a container share
/// a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecKind {
    Resolution,
    VideoCodec,
    BitDepth,
    Hdr,
    Audio,
    Bitrate,
    Container,
    Size,
}

/// One cell of the plate. Label-agnostic by design -- the strip renders
/// values only (`1080P`, `AAC 5.1`), never `RESOLUTION: 1080P`; the field
/// identity is carried by position and by the value's own shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpecField {
    pub value: String,
    pub weight: SpecWeight,
    pub kind: SpecKind,
}

impl SpecField {
    /// The **only** way to build a cell -- and it takes no weight, by
    /// design. §5's "data-driven, never hard-coded per row" can't be
    /// violated by a caller that has no way to express a violation.
    fn new(value: impl Into<String>, kind: SpecKind) -> Self {
        let value = value.into();
        let weight = classify(&value);
        SpecField {
            value,
            weight,
            kind,
        }
    }
}

/// §5's three-tier classifier, applied to an already-uppercased field value.
///
/// Deliberately substring-based on the *value* rather than keyed off the
/// field slot it came from: `AAC 5.1` and `TRUEHD 7.1 ATMOS` arrive in the
/// same slot, `10-BIT` shares its slot with `8-BIT`, and a future slot (a
/// playback-method cell reading `DIRECT PLAY`) must classify correctly the
/// day it is added without touching this function's callers.
///
/// The tiers, from §5's own table, read strongest-first so a value that
/// qualifies for two (`TRUEHD 7.1 ATMOS` is both lossless and Atmos) lands
/// on the higher one.
pub(crate) fn classify(value: &str) -> SpecWeight {
    /// §5 tier 3, "best in class": the formats that make a file the best
    /// copy of that title a person is likely to own. Bit depth is handled
    /// separately below (it's numeric, not a name).
    const BEST_IN_CLASS: [&str; 6] = [
        "DOLBY VISION",
        "ATMOS",
        "DTS:X",
        "DTS-HD",
        "TRUEHD",
        "DTSHD",
    ];
    /// §5 tier 2, "worth noticing": better than the default, short of the
    /// top. Substring-matched, so `HDR10+`/`HDR10` both hit `HDR`.
    const NOTABLE: [&str; 9] = [
        "4K",
        "8K",
        "2160P",
        "4320P",
        "HDR",
        "HLG",
        "HEVC",
        "AV1",
        "DIRECT PLAY",
    ];

    if BEST_IN_CLASS.iter().any(|n| value.contains(n)) {
        return SpecWeight::BestInClass;
    }
    // 10-BIT and up (but not 8-BIT) -- §5 names 10-bit in the top tier.
    if let Some(depth) = value.strip_suffix("-BIT") {
        if depth.parse::<u32>().is_ok_and(|d| d >= 10) {
            return SpecWeight::BestInClass;
        }
    }
    if NOTABLE.iter().any(|n| value.contains(n)) {
        return SpecWeight::Notable;
    }
    // Lossless audio: notable, not best-in-class -- a FLAC/PCM stereo track
    // is better than AC3 but is not the DTS-HD/Atmos tier §5 reserves the
    // accent for. Matched as whole words so a container or profile string
    // that merely *contains* one doesn't trip this.
    if value.contains("LOSSLESS")
        || value
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| matches!(word, "FLAC" | "PCM" | "ALAC"))
    {
        return SpecWeight::Notable;
    }
    SpecWeight::Baseline
}

/// The subset of a `BaseItemDto`/`MediaSourceInfo` the strip and the
/// breakdown read, resolved once so both derivations agree about *which*
/// streams they're describing. Borrowed, never cloned -- this is built
/// fresh inside a render pass.
pub(crate) struct MediaFacts<'a> {
    pub streams: &'a [MediaStream],
    pub container: Option<&'a str>,
    pub size: Option<i64>,
    pub bitrate: Option<i32>,
    pub path: Option<&'a str>,
    /// `BaseItemDto`'s own top-level `Width`/`Height`, used only as a
    /// resolution fallback when there is no video *stream* to read it from
    /// -- the entire browse path, since the mirror's bulk sync doesn't
    /// request `MediaStreams`/`MediaSources` (see `media_cache::sync::
    /// item_fields`), so a Library grid cell has only these two numbers.
    /// `None` for `from_source` (a `MediaSourceInfo` always has its streams).
    pub fallback_width: Option<i32>,
    pub fallback_height: Option<i32>,
}

impl<'a> MediaFacts<'a> {
    /// Detail-page entry point. Prefers the first `MediaSource`'s own
    /// streams (populated by the live `Fields=MediaStreams,MediaSources`
    /// enrichment fetch) and falls back to the DTO's top-level
    /// `MediaStreams` -- the mirror's cheaper bulk sync populates one or the
    /// other depending on how the page was reached, and neither is worth
    /// showing an empty strip over.
    pub(crate) fn from_dto(dto: &'a BaseItemDto) -> Self {
        let source = dto.media_sources.first();
        let streams = match source {
            Some(s) if !s.media_streams.is_empty() => s.media_streams.as_slice(),
            _ => dto.media_streams.as_slice(),
        };
        MediaFacts {
            streams,
            container: source
                .and_then(|s| s.container.as_deref())
                .or(dto.container.as_deref()),
            size: source.and_then(|s| s.size),
            bitrate: source.and_then(|s| s.bitrate),
            path: source.and_then(|s| s.path.as_deref()),
            fallback_width: dto.width,
            fallback_height: dto.height,
        }
    }

    /// Player entry point -- the exact `MediaSourceInfo` `decide_playback`
    /// picked for this session (see `playback.rs::PlaybackStarted::
    /// media_source`), so the OSD describes the stream actually playing
    /// rather than the item's first source.
    pub(crate) fn from_source(source: &'a MediaSourceInfo) -> Self {
        MediaFacts {
            streams: source.media_streams.as_slice(),
            container: source.container.as_deref(),
            size: source.size,
            bitrate: source.bitrate,
            path: source.path.as_deref(),
            fallback_width: None,
            fallback_height: None,
        }
    }

    /// The resolution cell: the video stream's own dimensions when there is
    /// a stream, else the DTO-level fallback (see `fallback_width`).
    fn resolution(&self) -> Option<String> {
        self.video()
            .and_then(resolution_label)
            .or_else(|| resolution_label_for(self.fallback_width, self.fallback_height))
    }

    fn video(&self) -> Option<&'a MediaStream> {
        self.streams
            .iter()
            .find(|s| s.type_ == Some(MediaStreamType::Video))
    }

    /// The default audio track if the server marks one, else the first --
    /// same selection `detail.rs::codec_badges` used before this component
    /// replaced it.
    fn audio(&self) -> Option<&'a MediaStream> {
        self.streams
            .iter()
            .find(|s| s.type_ == Some(MediaStreamType::Audio) && s.is_default == Some(true))
            .or_else(|| {
                self.streams
                    .iter()
                    .find(|s| s.type_ == Some(MediaStreamType::Audio))
            })
    }

    /// §2's field order: RESOLUTION │ VIDEO CODEC │ BIT DEPTH │ HDR FORMAT │
    /// AUDIO CODEC (channels appended) │ BITRATE │ CONTAINER │ SIZE.
    /// Unknown fields are **omitted**, never placeholdered.
    pub(crate) fn fields(&self) -> Vec<SpecField> {
        let video = self.video();
        let audio = self.audio();
        let mut out = Vec::new();
        if let Some(label) = self.resolution() {
            out.push(SpecField::new(label, SpecKind::Resolution));
        }
        if let Some(codec) = video.and_then(|v| v.codec.as_deref()) {
            out.push(SpecField::new(codec.to_uppercase(), SpecKind::VideoCodec));
        }
        if let Some(depth) = video.and_then(|v| v.bit_depth) {
            out.push(SpecField::new(format!("{depth}-BIT"), SpecKind::BitDepth));
        }
        if let Some(hdr) = video.and_then(hdr_label) {
            out.push(SpecField::new(hdr, SpecKind::Hdr));
        }
        if let Some(label) = audio.and_then(audio_label) {
            out.push(SpecField::new(label, SpecKind::Audio));
        }
        if let Some(bitrate) = self.bitrate.filter(|b| *b > 0) {
            out.push(SpecField::new(
                format!("{:.1} MBPS", bitrate as f64 / 1_000_000.0),
                SpecKind::Bitrate,
            ));
        }
        if let Some(container) = self.container.filter(|c| !c.is_empty()) {
            out.push(SpecField::new(
                container.to_uppercase(),
                SpecKind::Container,
            ));
        }
        if let Some(size) = self.size.filter(|s| *s > 0) {
            out.push(SpecField::new(format_bytes(size), SpecKind::Size));
        }
        out
    }

    /// §5's poster-hover variant: resolution + HDR format + audio format
    /// only -- enough to make a library grid browsable by quality without
    /// covering the artwork. Wired by `root.rs::build_library_state` ->
    /// `grid.rs::poster_grid` -> `cards.rs::poster_spec_overlay`.
    ///
    /// **Honest limitation on the browse path**: the only DTO available
    /// there is the mirror's bulk-sync blob, which carries no
    /// `MediaStreams`/`MediaSources` at all, so in practice only the
    /// resolution cell resolves (via `fallback_width`/`fallback_height`) and
    /// the HDR/audio cells are simply absent -- omitted, per this module's
    /// no-placeholders rule, not rendered blank. The same call on a Detail
    /// page's enriched DTO returns all three.
    pub(crate) fn condensed_fields(&self) -> Vec<SpecField> {
        let video = self.video();
        let mut out = Vec::new();
        if let Some(label) = self.resolution() {
            out.push(SpecField::new(label, SpecKind::Resolution));
        }
        if let Some(hdr) = video.and_then(hdr_label) {
            out.push(SpecField::new(hdr, SpecKind::Hdr));
        }
        if let Some(label) = self.audio().and_then(audio_label) {
            out.push(SpecField::new(label, SpecKind::Audio));
        }
        out
    }

    /// The MediaInfo-style full breakdown behind the strip's ⓘ affordance:
    /// one section per stream (video/audio/subtitle) plus a trailing FILE
    /// section carrying container, overall bitrate, size and the file path.
    /// Pure (returns data, not elements) so the section/row derivation is
    /// unit-testable without a GPUI window.
    pub(crate) fn breakdown(&self) -> Vec<BreakdownSection> {
        let mut sections: Vec<BreakdownSection> = Vec::new();
        let mut counts = (0usize, 0usize, 0usize);
        for stream in self.streams {
            let (title, rows) = match stream.type_ {
                Some(MediaStreamType::Video) => {
                    counts.0 += 1;
                    (format!("VIDEO {}", counts.0), video_rows(stream))
                }
                Some(MediaStreamType::Audio) => {
                    counts.1 += 1;
                    (format!("AUDIO {}", counts.1), audio_rows(stream))
                }
                Some(MediaStreamType::Subtitle) => {
                    counts.2 += 1;
                    (format!("SUBTITLE {}", counts.2), subtitle_rows(stream))
                }
                // EmbeddedImage/Data/Lyric/unknown: not media the viewer
                // is choosing between, skipped rather than shown as an
                // unlabeled section.
                _ => continue,
            };
            sections.push(BreakdownSection { title, rows });
        }
        let mut file_rows = Vec::new();
        push_row(
            &mut file_rows,
            "CONTAINER",
            self.container.map(str::to_uppercase),
        );
        push_row(
            &mut file_rows,
            "BITRATE",
            self.bitrate
                .filter(|b| *b > 0)
                .map(|b| format!("{:.1} MBPS", b as f64 / 1_000_000.0)),
        );
        push_row(
            &mut file_rows,
            "SIZE",
            self.size.filter(|s| *s > 0).map(format_bytes),
        );
        push_row(&mut file_rows, "PATH", self.path.map(str::to_string));
        if !file_rows.is_empty() {
            sections.push(BreakdownSection {
                title: "FILE".to_string(),
                rows: file_rows,
            });
        }
        sections
    }
}

/// One labeled group of `(label, value)` rows in the full breakdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BreakdownSection {
    pub title: String,
    pub rows: Vec<(String, String)>,
}

fn push_row(rows: &mut Vec<(String, String)>, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        rows.push((label.to_string(), value));
    }
}

fn video_rows(s: &MediaStream) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    push_row(
        &mut rows,
        "CODEC",
        s.codec.as_ref().map(|c| c.to_uppercase()),
    );
    push_row(&mut rows, "PROFILE", profile_level(s));
    push_row(
        &mut rows,
        "RESOLUTION",
        match (s.width, s.height) {
            (Some(w), Some(h)) => Some(format!("{w}x{h}")),
            _ => None,
        },
    );
    push_row(&mut rows, "HDR", hdr_label(s));
    push_row(
        &mut rows,
        "BIT DEPTH",
        s.bit_depth.map(|d| format!("{d}-BIT")),
    );
    push_row(
        &mut rows,
        "FRAME RATE",
        s.average_frame_rate
            .or(s.real_frame_rate)
            .filter(|f| *f > 0.0)
            .map(|f| format!("{f:.3} FPS")),
    );
    push_row(
        &mut rows,
        "BITRATE",
        s.bit_rate
            .filter(|b| *b > 0)
            .map(|b| format!("{:.1} MBPS", b as f64 / 1_000_000.0)),
    );
    push_row(&mut rows, "LANGUAGE", language_label(s));
    rows
}

fn audio_rows(s: &MediaStream) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    push_row(
        &mut rows,
        "CODEC",
        s.codec.as_ref().map(|c| c.to_uppercase()),
    );
    push_row(&mut rows, "PROFILE", profile_level(s));
    push_row(
        &mut rows,
        "CHANNELS",
        channel_label(s).map(|c| match s.channels {
            Some(n) => format!("{c} ({n} CH)"),
            None => c,
        }),
    );
    push_row(
        &mut rows,
        "SAMPLE RATE",
        s.sample_rate
            .filter(|r| *r > 0)
            .map(|r| format!("{:.1} KHZ", r as f64 / 1000.0)),
    );
    push_row(
        &mut rows,
        "BIT DEPTH",
        s.bit_depth.map(|d| format!("{d}-BIT")),
    );
    push_row(
        &mut rows,
        "BITRATE",
        s.bit_rate
            .filter(|b| *b > 0)
            .map(|b| format!("{:.0} KBPS", b as f64 / 1000.0)),
    );
    push_row(&mut rows, "LANGUAGE", language_label(s));
    rows
}

fn subtitle_rows(s: &MediaStream) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    push_row(
        &mut rows,
        "CODEC",
        s.codec.as_ref().map(|c| c.to_uppercase()),
    );
    push_row(&mut rows, "LANGUAGE", language_label(s));
    // Rips often ship subtitle tracks with NO language tag in the
    // container; when that happens the server's per-track
    // Title/DisplayTitle ("English (SDH)", "Commentary") is usually the
    // only human-usable label it has -- surface it rather than leaving the
    // track anonymous. Skipped when it would just repeat the language row.
    let title = s
        .title
        .as_ref()
        .or(s.display_title.as_ref())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_uppercase())
        .filter(|t| language_label(s).is_none_or(|l| !t.contains(&l)));
    push_row(&mut rows, "TITLE", title);
    let mut flags = Vec::new();
    if s.is_default == Some(true) {
        flags.push("DEFAULT");
    }
    if s.is_forced == Some(true) {
        flags.push("FORCED");
    }
    if s.is_external == Some(true) {
        flags.push("EXTERNAL");
    }
    if s.is_hearing_impaired == Some(true) {
        flags.push("SDH");
    }
    push_row(
        &mut rows,
        "FLAGS",
        (!flags.is_empty()).then(|| flags.join(" · ")),
    );
    rows
}

/// `Main 10 @ L5.1` -- `MediaStream::level` is an `f64` carrying the
/// integer-scaled H.264/HEVC level (51 => 5.1), the same encoding ffprobe
/// reports.
fn profile_level(s: &MediaStream) -> Option<String> {
    let profile = s.profile.as_ref().filter(|p| !p.is_empty());
    let level = s.level.filter(|l| *l > 0.0);
    match (profile, level) {
        (Some(p), Some(l)) => Some(format!("{} @ L{}", p.to_uppercase(), fmt_level(l))),
        (Some(p), None) => Some(p.to_uppercase()),
        (None, Some(l)) => Some(format!("L{}", fmt_level(l))),
        (None, None) => None,
    }
}

fn fmt_level(level: f64) -> String {
    if level >= 10.0 {
        format!("{:.1}", level / 10.0)
    } else {
        format!("{level:.0}")
    }
}

/// Server-configured language names are shown verbatim (localized name if
/// the server sent one, else the raw ISO code) -- never rewritten.
fn language_label(s: &MediaStream) -> Option<String> {
    s.localized_language
        .as_ref()
        .or(s.language.as_ref())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_uppercase())
}

fn resolution_label(v: &MediaStream) -> Option<String> {
    resolution_label_for(v.width, v.height)
}

/// The bucket ladder itself, taken as plain dimensions so both a
/// `MediaStream`'s own width/height and the DTO-level fallback (§5's browse
/// path -- see `MediaFacts::fallback_width`) run through exactly one
/// classifier.
/// A height-first ladder below 4K demotes any widescreen crop a tier (e.g.
/// a 1920x960 scope-cropped 1080p source reads as "720P"), so every tier
/// promotes on EITHER axis -- width thresholds first, since a cropped
/// frame keeps its width; height still promotes so 4:3 content like
/// 1440x1080 keeps its tier.
fn resolution_label_for(width: Option<i32>, height: Option<i32>) -> Option<String> {
    let h = height.filter(|h| *h > 0)?;
    let w = width.unwrap_or(0);
    let label = if w >= 7000 || h >= 4000 {
        "8K"
    } else if w >= 3500 || h >= 2000 {
        "4K"
    } else if w >= 2400 || h >= 1400 {
        "1440P"
    } else if w >= 1800 || h >= 1000 {
        "1080P"
    } else if w >= 1200 || h >= 700 {
        "720P"
    } else if w >= 1000 || h >= 560 {
        "576P"
    } else {
        "SD"
    };
    Some(label.to_string())
}

/// HDR flavor from `VideoRangeType` (the precise field), falling back to
/// `VideoRange`'s coarse SDR/HDR split when the server only sent that.
/// Every Dolby Vision variant collapses to `DOLBY VISION` -- the cross-
/// compatibility suffixes (`DOVIWithHDR10`, ...) are a mastering detail the
/// strip has no room for, and the full breakdown shows the same field.
fn hdr_label(v: &MediaStream) -> Option<String> {
    let from_type = match v.video_range_type {
        Some(VideoRangeType::Hdr10) => Some("HDR10"),
        Some(VideoRangeType::Hdr10Plus) => Some("HDR10+"),
        Some(VideoRangeType::Hlg) => Some("HLG"),
        Some(
            VideoRangeType::Dovi
            | VideoRangeType::DoviWithHdr10
            | VideoRangeType::DoviWithHlg
            | VideoRangeType::DoviWithSdr
            | VideoRangeType::DoviWithEl
            | VideoRangeType::DoviWithHdr10Plus
            | VideoRangeType::DoviWithElhdr10Plus,
        ) => Some("DOLBY VISION"),
        _ => None,
    };
    from_type
        .map(str::to_string)
        .or_else(|| (v.video_range == Some(VideoRange::Hdr)).then(|| "HDR".to_string()))
}

/// `AAC 5.1`, `TRUEHD 7.1 ATMOS` -- codec with the channel layout appended,
/// plus a spatial-format suffix when the server reports one.
fn audio_label(a: &MediaStream) -> Option<String> {
    let mut label = a.codec.as_ref().filter(|c| !c.is_empty())?.to_uppercase();
    if let Some(channels) = channel_label(a) {
        label.push(' ');
        label.push_str(&channels);
    }
    match a.audio_spatial_format {
        Some(AudioSpatialFormat::DolbyAtmos) => label.push_str(" ATMOS"),
        Some(AudioSpatialFormat::Dtsx) => label.push_str(" DTS:X"),
        _ => {
            // Older servers carry Atmos only in the profile string
            // (`TrueHD Atmos 7.1`), so recover it from there rather than
            // silently dropping the single most noteworthy audio fact.
            if a.profile
                .as_deref()
                .is_some_and(|p| p.to_uppercase().contains("ATMOS"))
                && !label.contains("ATMOS")
            {
                label.push_str(" ATMOS");
            }
        }
    }
    Some(label)
}

/// The server's own `ChannelLayout` verbatim when present (`5.1`, `7.1`),
/// else derived from the raw channel count.
fn channel_label(a: &MediaStream) -> Option<String> {
    if let Some(layout) = a.channel_layout.as_ref().filter(|l| !l.is_empty()) {
        return Some(layout.to_uppercase());
    }
    match a.channels? {
        1 => Some("MONO".to_string()),
        2 => Some("STEREO".to_string()),
        6 => Some("5.1".to_string()),
        8 => Some("7.1".to_string()),
        n if n > 0 => Some(format!("{n} CH")),
        _ => None,
    }
}

/// `531.3 MB`. Moved here from `detail.rs` (its only caller was the
/// `file_info_line` this component replaces).
pub(crate) fn format_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

/// §5's ` │ ` field separator as an element. `flex_none` so a wrapping strip
/// never squeezes one to nothing, and its own quiet [`theme::SPEC_SEPARATOR`]
/// tone so the separators sit *behind* even a baseline value.
pub(crate) fn spec_separator() -> Div {
    div()
        .flex_none()
        .text_color(rgba(theme::SPEC_SEPARATOR))
        .child(SharedString::from(SEPARATOR))
}

/// One cell of the label. No padding of its own -- §5 spaces the values with
/// the separator's own surrounding spaces, and the pill supplies the outer
/// inset -- and no fill or radius; the tier colour is the entire treatment.
pub(crate) fn spec_cell(value: impl Into<SharedString>, weight: SpecWeight) -> Div {
    div().text_color(rgb(weight.color())).child(value.into())
}

/// Monospace advance width at [`theme::TEXT_SPEC`] (10px). Martian Mono's
/// advance is a fixed 0.70em (checked against the vendored TTF's `hmtx`
/// against a 1000 `unitsPerEm`), so `10 * 0.70 = 7.0` exactly. Only valid
/// *because* the strip is monospace: this is a real metric for this one
/// family at this one size, not a general text-measurement substitute.
const MONO_ADVANCE: f32 = 7.0;
/// The pill's own horizontal inset, per side.
const PILL_PAD: f32 = 12.0;
/// What `spec_info_button` costs the row it shares with the pill, now that
/// it renders *outside* the pill (see that fn's doc comment): the caller's
/// 12px gap plus the four mono columns of `"INFO"`. It no longer costs a
/// ` │ ` separator or a second helping of the pill's own inset, because it
/// is no longer inside the pill.
pub(crate) const INFO_CELL_W: f32 = INFO_GAP + 4.0 * MONO_ADVANCE;
/// The gap between the pill and the INFO control beside it.
pub(crate) const INFO_GAP: f32 = 12.0;

/// The strip's laid-out width in px, for a given field set.
///
/// GPUI 0.2.2 has no measure/`on_layout` API -- there is no way to ask what
/// a rendered strip's width is, and no way to be told where a `flex_wrap`
/// row actually broke. That rules out the "detect the wrap point and skip
/// the hairline there" treatment outright. What it does *not* rule out is
/// computing the width ourselves: every cell is monospace at a fixed size
/// with fixed padding, so the width is exactly
/// `chars * MONO_ADVANCE + padding`, no font callback required.
pub(crate) fn strip_width_px(fields: &[SpecField], trailing_info: bool) -> f32 {
    if fields.is_empty() && !trailing_info {
        return 0.0;
    }
    // The pill's own inset, both sides.
    let mut w = PILL_PAD * 2.0;
    for (ix, field) in fields.iter().enumerate() {
        if ix > 0 {
            w += SEPARATOR_COLS * MONO_ADVANCE; // §5's ` │ `
        }
        w += field.value.chars().count() as f32 * MONO_ADVANCE;
    }
    if trailing_info {
        w += INFO_CELL_W;
    }
    w
}

/// Round 3 ("responsiveness loses information"): split the cells into as
/// many ROWS as the width demands, in order, never dropping any -- §6's
/// "never hide a spec value" outranks the tidy-single-row ideal, and the
/// earlier drop-to-fit behaviour (SIZE, then CONTAINER, then BITRATE) was
/// exactly what a narrow window lost `5.4 MBPS │ MKV │ 734.8 MB` to.
///
/// Greedy first-fit in display order, so a wide container still yields the
/// single classic row. `trailing_info` reserves the INFO control's width
/// beside the LAST row only (that is where the caller mounts it): if the
/// last row can't host it, its final field moves down to a fresh row --
/// the same anti-orphan guarantee the old fit gave (`802.8 MB | ⓘ INFO`
/// alone on a line), now by reflow instead of by deletion. A single field
/// wider than the container overflows its own row rather than being cut:
/// the value is the product.
pub(crate) fn partition_fields(
    fields: &[SpecField],
    avail_px: f32,
    trailing_info: bool,
) -> Vec<Vec<SpecField>> {
    let mut rows: Vec<Vec<SpecField>> = Vec::new();
    for field in fields {
        match rows.last_mut() {
            Some(row) => {
                let mut candidate = row.clone();
                candidate.push(field.clone());
                if strip_width_px(&candidate, false) <= avail_px {
                    *row = candidate;
                } else {
                    rows.push(vec![field.clone()]);
                }
            }
            None => rows.push(vec![field.clone()]),
        }
    }
    if trailing_info {
        // Reflow the tail until the INFO control fits beside the last row
        // (or that row is down to one field, at which point it overflows
        // rather than lying).
        while let Some(last) = rows.last() {
            if strip_width_px(last, true) <= avail_px || last.len() <= 1 {
                break;
            }
            let mut last = rows.pop().expect("checked non-empty");
            let moved = last.pop().expect("checked len > 1");
            rows.push(last);
            rows.push(vec![moved]);
        }
    }
    rows
}

/// §5's pill. Returns a `Div` (not `impl IntoElement`) so a caller can
/// append its own trailing children -- the INFO affordance is mounted that
/// way (`spec_separator()` + `spec_info_button()`), keeping the separator
/// rhythm identical to the fields' own.
///
/// Width-unaware: renders every field it is given, and `flex_wrap()` is the
/// fallback when they don't fit. Callers that know their own width should
/// use [`spec_strip_fitted`] instead -- this is that function with
/// "infinitely wide", so the two share one rendering path.
///
/// Currently unconsumed: the two live callers both know their own width
/// (Detail's text column, the OSD's viewport) and use `spec_strip_fitted`,
/// and the third (the library wall's poster hover) uses
/// [`spec_strip_condensed`]. Kept as the component's plain, width-unaware
/// entry point rather than deleted and re-added verbatim by the next screen
/// that wants a strip without a measurement to hand.
#[allow(dead_code)]
pub(crate) fn spec_strip(fields: &[SpecField]) -> Div {
    spec_strip_fitted(fields, f32::INFINITY, false)
}

/// The **poster-hover** variant: same tiers, same ` │ ` separators, same
/// face and size -- but without §5's pill chrome (fill, border, 12px
/// insets).
///
/// Stated plainly, because it is the one place this component departs from
/// §5's container spec: a library-wall cell is ~150-180px wide, and the pill
/// costs 24px of inset plus 2px of border before a single character is set.
/// A three-cell condensed strip (`4K │ HDR10 │ DTS-HD MA 7.1`) does not fit
/// inside one at any size the brief's 10-11px range allows, so a pill here
/// would either clip a spec value -- which §6 forbids outright ("never hide
/// a spec value to make a row look tidier") -- or wrap into a two-row pill
/// covering a third of the artwork.
///
/// Dropping the chrome rather than the data is the right trade because the
/// chrome's *job* is already done by something else here: the pill's fill
/// exists so the strip reads as a label rather than as loose text, and
/// `cards::poster_spec_overlay` already paints this strip on its own
/// near-opaque `NOTTE` ramp for exactly that reason.
pub(crate) fn spec_strip_condensed(fields: &[SpecField]) -> Div {
    spec_strip_inner(fields.to_vec(), false)
}

/// [`spec_strip`] for a caller that knows how much horizontal room it has:
/// the fields are partitioned into rows (see [`partition_fields`]) and each
/// row renders as its own §5 pill, stacked -- every value always present.
/// `avail_px` is the strip's own content width -- the container's width
/// minus its padding.
///
/// `trailing_info` must be `true` when the caller is going to place
/// `spec_info_button()` beside the last pill, so the partition reserves
/// that control's width ([`INFO_CELL_W`]) on the last row. Callers needing
/// to mount the control themselves should use [`spec_strip_pill_rows`] and
/// compose; this builder returns the stacked pills alone.
pub(crate) fn spec_strip_fitted(fields: &[SpecField], avail_px: f32, trailing_info: bool) -> Div {
    let mut rows = spec_strip_pill_rows(fields, avail_px, trailing_info);
    if rows.len() == 1 {
        return rows.remove(0);
    }
    let mut col = div().flex().flex_col().gap(px(PILL_ROW_GAP));
    for row in rows {
        col = col.child(row);
    }
    col
}

/// The stacked pills as individual `Div`s, for a caller that composes its
/// own final row (the Detail page mounts `spec_info_button()` beside the
/// last one). One pill per [`partition_fields`] row.
pub(crate) fn spec_strip_pill_rows(
    fields: &[SpecField],
    avail_px: f32,
    trailing_info: bool,
) -> Vec<Div> {
    partition_fields(fields, avail_px, trailing_info)
        .into_iter()
        .map(|row| spec_strip_inner(row, true))
        .collect()
}

/// Vertical gap between stacked pill rows when the strip needs more than
/// one -- tight, so the stack still reads as one label.
pub(crate) const PILL_ROW_GAP: f32 = 6.0;

/// The one rendering path both public builders share. `chrome` is §5's pill
/// (fill + hairline border + insets); see [`spec_strip_condensed`] for the
/// only context that turns it off and why.
fn spec_strip_inner(fields: Vec<SpecField>, chrome: bool) -> Div {
    // §5's exact container: "A 999px pill, `SURFACE` fill, 1px `HAIRLINE`
    // border, Martian Mono 10-11px". `align_self: Start` so the pill hugs
    // its own content instead of stretching to the full width of whatever
    // column it is dropped into -- a full-width pill reads as a bar, not a
    // label. Set through `Styled::style()` because gpui 0.2.2 exposes
    // `items_*` (align-items, for a container's children) but no
    // `self_*`/align-self fluent method of its own.
    let mut pill = div();
    pill.style().align_self = Some(gpui::AlignSelf::Start);
    let mut row = theme::apply_tabular_nums(
        pill.flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .font_family(MONO_FAMILY)
            .text_size(theme::TEXT_SPEC)
            .when(chrome, |d| {
                d.px(px(PILL_PAD))
                    .py(theme::SPACE_COMPACT)
                    .rounded(theme::RADIUS_PILL)
                    .bg(rgb(theme::SURFACE_RAISED))
                    .border_1()
                    .border_color(rgb(theme::SURFACE_HAIRLINE))
            }),
    );
    for (ix, field) in fields.iter().enumerate() {
        // A cell and the separator that introduces it are ONE flex item, not
        // two. As two siblings, a wrap could fall between them and leave the
        // separator dangling at the end of a row with nothing after it.
        // Wrapped inside a `flex_none` pair, the separator physically
        // cannot outlive its own value.
        //
        // A wrapped row can still *begin* with a separator-led cell. GPUI
        // 0.2.2 cannot report where a `flex_wrap` row broke (no measure or
        // post-layout callback), so suppressing that one is not achievable
        // at this layer -- which is precisely why `spec_strip_fitted`
        // exists: the real fix is not wrapping in the first place.
        row = row.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                .when(ix > 0, |d| d.child(spec_separator()))
                .child(spec_cell(field.value.clone(), field.weight)),
        );
    }
    row
}

/// The info affordance that sits **beside** the pill.
///
/// It used to be the strip's last cell, separator-led, inside the pill
/// itself. That was wrong on the brief's own terms: §5 defines the spec
/// strip as the label of file *facts* -- resolution, codec, bit depth,
/// audio, bitrate, container -- and "INFO" is not one of them, it is a
/// control. Inside the pill it read as an eighth spec value that happened
/// to be clickable. It now renders outside the pill, on the same row, with
/// its own mono face and size (it no longer inherits them from the pill it
/// used to live in) at `GRIGIO`, brightening to `PANNA` on hover. The
/// caller owns the 12px gap and the `on_click`; `open` renders it at the
/// notable tier so the affordance shows which state it's in.
///
/// Reads `INFO`, not `ⓘ INFO`: Martian Mono has no U+24D8 glyph (checked
/// against the vendored TTF's `cmap`), so the circled-i would have been
/// substituted mid-strip from an unrelated face at an unrelated advance --
/// and §6's "no illustration" reading favours the bare word anyway.
///
/// The info affordance must read as clickable. The label language is
/// deliberately fill-less, which is exactly what made this
/// read as another inert spec cell: at rest it is the same mono/uppercase/
/// `GRIGIO` as the value beside it, and a rest-state difference would break
/// the label. So the affordance is carried on *hover* instead, three ways at
/// once, plus a real hit target:
///
/// * `cursor_pointer` (was already here),
/// * text brightens to `PANNA` **and** gains an underline -- the underline
///   is the part that says "control", since brightening alone is
///   indistinguishable from the strip's own `Notable` tier,
/// * a faint wash so the hit area itself is visible,
/// * `min_h(24px)` + `items_center`, so the clickable box clears the
///   24px minimum rather than being the ~13px tall text line it was.
pub(crate) fn spec_info_button(id: impl Into<ElementId>, open: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        // Outside the pill it has to state its own type: §3's mono face at
        // the 10px spec size, which it previously inherited from the pill's
        // own `font_family`/`text_size`.
        .font_family(MONO_FAMILY)
        .text_size(theme::TEXT_SPEC)
        // Keeps the clickable box at the 24px minimum rather than the ~13px
        // text line it would otherwise be. No horizontal padding: the
        // caller's 12px gap is the whole separation from the pill, and
        // padding here would silently widen it.
        .min_h(px(24.))
        .cursor_pointer()
        .text_color(rgb(if open {
            theme::TEXT_SPEC_NOTABLE
        } else {
            theme::TEXT_SPEC_BASELINE
        }))
        .when(open, |d| d.underline())
        .hover(|s| s.text_color(rgb(theme::TEXT_SPEC_NOTABLE)).underline())
        .child(SharedString::from("INFO"))
}

/// The full MediaInfo-style breakdown body: every stream with its language,
/// codec, profile+level, frame rate, sample rate, bit depth and channel
/// layout, then container/bitrate/size/file path. Monospace throughout --
/// this is the one place the file path lives now (folded in from the old
/// page-bottom `path_footer`).
pub(crate) fn media_breakdown(sections: &[BreakdownSection]) -> Div {
    let mut body = div()
        .flex()
        .flex_col()
        .gap_3()
        .p_2()
        .font_family(MONO_FAMILY)
        .text_size(theme::TEXT_SPEC);
    for section in sections {
        let mut block = div().flex().flex_col().gap_1().child(
            div()
                .pb_1()
                .text_color(rgba(theme::TEXT_PRIMARY))
                .child(SharedString::from(section.title.clone())),
        );
        for (label, value) in &section.rows {
            block = block.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .w(px(92.))
                            .text_color(rgba(theme::TEXT_QUATERNARY))
                            .child(SharedString::from(label.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(rgb(theme::TEXT_SPEC_BASELINE))
                            .child(SharedString::from(value.clone())),
                    ),
            );
        }
        body = body.child(block);
    }
    theme::apply_tabular_nums(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins: widescreen crops classify by width, not height; tall aspect ratios still classify by height.
    #[test]
    fn widescreen_crops_classify_by_width_not_height() {
        let label = |w, h| resolution_label_for(Some(w), Some(h));
        assert_eq!(label(1920, 960).as_deref(), Some("1080P")); // 2.0:1 (the field file)
        assert_eq!(label(1920, 800).as_deref(), Some("1080P")); // 2.40:1 scope
        assert_eq!(label(1920, 1080).as_deref(), Some("1080P"));
        assert_eq!(label(1280, 534).as_deref(), Some("720P")); // 720p scope
        assert_eq!(label(3840, 1600).as_deref(), Some("4K")); // 4K scope
        assert_eq!(label(1440, 1080).as_deref(), Some("1080P")); // 4:3 via height
        assert_eq!(label(720, 576).as_deref(), Some("576P")); // PAL DVD
        assert_eq!(label(640, 480).as_deref(), Some("SD"));
    }

    /// `BaseItemDto`/`MediaStream` are OpenAPI-generated with no `Default`
    /// impl, so fixtures are built through `serde_json` -- the same
    /// convention `detail.rs`'s own tests use.
    fn stream(json: serde_json::Value) -> MediaStream {
        serde_json::from_value(json).expect("stream fixture")
    }

    fn source(json: serde_json::Value) -> MediaSourceInfo {
        serde_json::from_value(json).expect("source fixture")
    }

    fn full_source() -> MediaSourceInfo {
        source(serde_json::json!({
            "Container": "mkv",
            "Size": 557_000_000i64,
            "Bitrate": 3_400_000,
            "Path": "/media/Movies/Example (2019)/Example.mkv",
            "MediaStreams": [
                {
                    "Type": "Video",
                    "Codec": "hevc",
                    "Width": 1920,
                    "Height": 1080,
                    "BitDepth": 10,
                    "VideoRangeType": "HDR10",
                    "Profile": "Main 10",
                    "Level": 51.0,
                    "AverageFrameRate": 23.976,
                    "Language": "eng"
                },
                {
                    "Type": "Audio",
                    "Codec": "aac",
                    "ChannelLayout": "5.1",
                    "Channels": 6,
                    "SampleRate": 48000,
                    "IsDefault": true,
                    "Language": "eng"
                },
                { "Type": "Subtitle", "Codec": "subrip", "Language": "eng", "IsForced": true }
            ]
        }))
    }

    // ---- field derivation ------------------------------------------

    #[test]
    fn fields_follow_spec_order() {
        let src = full_source();
        let values: Vec<String> = MediaFacts::from_source(&src)
            .fields()
            .into_iter()
            .map(|f| f.value)
            .collect();
        assert_eq!(
            values,
            vec!["1080P", "HEVC", "10-BIT", "HDR10", "AAC 5.1", "3.4 MBPS", "MKV", "531.2 MB"]
        );
    }

    #[test]
    fn unknown_fields_are_omitted_never_placeholdered() {
        // Only a codec is known -- everything else must simply not appear.
        let src = source(serde_json::json!({
            "MediaStreams": [{ "Type": "Video", "Codec": "h264" }]
        }));
        let values: Vec<String> = MediaFacts::from_source(&src)
            .fields()
            .into_iter()
            .map(|f| f.value)
            .collect();
        assert_eq!(values, vec!["H264"]);
    }

    #[test]
    fn empty_source_yields_an_empty_strip_not_a_row_of_blanks() {
        let src = source(serde_json::json!({}));
        assert!(MediaFacts::from_source(&src).fields().is_empty());
    }

    /// The `merge_enrichment` behavior: a partially-populated DTO must
    /// still render whatever it has, and gain fields as enrichment lands.
    #[test]
    fn fields_grow_as_enrichment_lands() {
        let early: BaseItemDto = serde_json::from_value(serde_json::json!({
            "MediaStreams": [{ "Type": "Video", "Codec": "hevc", "Height": 2160 }]
        }))
        .expect("dto");
        assert_eq!(MediaFacts::from_dto(&early).fields().len(), 2);
        let enriched: BaseItemDto = serde_json::from_value(serde_json::json!({
            "MediaSources": [{
                "Container": "mkv",
                "MediaStreams": [{ "Type": "Video", "Codec": "hevc", "Height": 2160 }]
            }]
        }))
        .expect("dto");
        assert_eq!(MediaFacts::from_dto(&enriched).fields().len(), 3);
    }

    #[test]
    fn audio_field_appends_channels_and_atmos() {
        let src = source(serde_json::json!({
            "MediaStreams": [{
                "Type": "Audio", "Codec": "truehd", "ChannelLayout": "7.1",
                "AudioSpatialFormat": "DolbyAtmos"
            }]
        }));
        let fields = MediaFacts::from_source(&src).fields();
        assert_eq!(fields[0].value, "TRUEHD 7.1 ATMOS");
    }

    #[test]
    fn audio_channels_derive_from_count_when_no_layout() {
        let src = source(serde_json::json!({
            "MediaStreams": [{ "Type": "Audio", "Codec": "flac", "Channels": 2 }]
        }));
        assert_eq!(
            MediaFacts::from_source(&src).fields()[0].value,
            "FLAC STEREO"
        );
    }

    #[test]
    fn default_audio_track_wins_over_first() {
        let src = source(serde_json::json!({
            "MediaStreams": [
                { "Type": "Audio", "Codec": "ac3", "Channels": 2 },
                { "Type": "Audio", "Codec": "dts", "Channels": 6, "IsDefault": true }
            ]
        }));
        assert_eq!(MediaFacts::from_source(&src).fields()[0].value, "DTS 5.1");
    }

    #[test]
    fn condensed_is_resolution_hdr_and_audio_only() {
        let src = full_source();
        let values: Vec<String> = MediaFacts::from_source(&src)
            .condensed_fields()
            .into_iter()
            .map(|f| f.value)
            .collect();
        assert_eq!(values, vec!["1080P", "HDR10", "AAC 5.1"]);
    }

    /// Pins: a bulk-synced blob with no streams still resolves the resolution cell (and only that cell) from the DTO's own `Width`/`Height`.
    #[test]
    fn condensed_falls_back_to_dto_dimensions_when_there_are_no_streams() {
        let dto: BaseItemDto = serde_json::from_value(serde_json::json!({
            "Width": 3840, "Height": 2160
        }))
        .expect("dto");
        let fields = MediaFacts::from_dto(&dto).condensed_fields();
        let values: Vec<&str> = fields.iter().map(|f| f.value.as_str()).collect();
        assert_eq!(values, vec!["4K"]);
        assert_eq!(fields[0].weight, SpecWeight::Notable);
    }

    #[test]
    fn dto_dimension_fallback_never_overrides_a_real_video_stream() {
        let dto: BaseItemDto = serde_json::from_value(serde_json::json!({
            "Width": 3840,
            "Height": 2160,
            "MediaStreams": [{ "Type": "Video", "Width": 1920, "Height": 1080 }]
        }))
        .expect("dto");
        assert_eq!(
            MediaFacts::from_dto(&dto).condensed_fields()[0].value,
            "1080P"
        );
    }

    #[test]
    fn no_dimensions_at_all_yields_no_condensed_fields() {
        let dto: BaseItemDto = serde_json::from_value(serde_json::json!({})).expect("dto");
        assert!(MediaFacts::from_dto(&dto).condensed_fields().is_empty());
    }

    #[test]
    fn dolby_vision_variants_collapse_to_one_label() {
        for range_type in ["DOVI", "DOVIWithHDR10", "DOVIWithELHDR10Plus"] {
            let s = stream(serde_json::json!({
                "Type": "Video", "Height": 2160, "VideoRangeType": range_type
            }));
            assert_eq!(hdr_label(&s).as_deref(), Some("DOLBY VISION"));
        }
    }

    #[test]
    fn coarse_video_range_is_the_fallback_when_no_range_type() {
        let s = stream(serde_json::json!({
            "Type": "Video", "Height": 2160, "VideoRange": "HDR"
        }));
        assert_eq!(hdr_label(&s).as_deref(), Some("HDR"));
        let sdr = stream(serde_json::json!({ "Type": "Video", "VideoRange": "SDR" }));
        assert_eq!(hdr_label(&sdr), None);
    }

    // ---- brand §5's three-tier classification -----------------------

    /// Tier 3, `PISTACCHIO`: §5's own examples plus the formats that belong
    /// in the same bracket (TrueHD, DTS:X).
    #[test]
    fn best_in_class_values_get_the_accent() {
        for value in [
            "10-BIT",
            "12-BIT",
            "DOLBY VISION",
            "TRUEHD 7.1 ATMOS",
            "EAC3 5.1 ATMOS",
            "DTS-HD MA 7.1",
            "DTS 7.1 DTS:X",
        ] {
            assert_eq!(
                classify(value),
                SpecWeight::BestInClass,
                "{value} should be best-in-class"
            );
        }
    }

    /// Tier 2, `PANNA`: better than the default, short of the top.
    #[test]
    fn notable_values_get_panna() {
        for value in [
            "4K",
            "8K",
            "2160P",
            "HDR10",
            "HDR10+",
            "HLG",
            "HEVC",
            "AV1",
            "DIRECT PLAY",
            "FLAC STEREO",
            "PCM STEREO",
            "ALAC STEREO",
        ] {
            assert_eq!(
                classify(value),
                SpecWeight::Notable,
                "{value} should be notable"
            );
        }
    }

    /// Tier 1, `GRIGIO`: §5's "ordinary values" list, verbatim, plus the
    /// near-misses that must NOT get promoted (`8-BIT` is not `10-BIT`;
    /// plain `DTS` is not `DTS-HD`).
    #[test]
    fn ordinary_values_stay_baseline() {
        for value in [
            "1080P",
            "720P",
            "SD",
            "H264",
            "8-BIT",
            "AAC 5.1",
            "AC3 5.1",
            "DTS 5.1",
            "MKV",
            "531.2 MB",
            "21.4 MBPS",
        ] {
            assert_eq!(
                classify(value),
                SpecWeight::Baseline,
                "{value} should be baseline"
            );
        }
    }

    /// A value that qualifies for two tiers must land on the higher one --
    /// `TRUEHD 7.1 ATMOS` is lossless (notable) *and* Atmos (best), and the
    /// accent is the honest answer.
    #[test]
    fn the_strongest_matching_tier_wins() {
        assert_eq!(classify("TRUEHD 7.1 ATMOS"), SpecWeight::BestInClass);
        assert_eq!(classify("HEVC"), SpecWeight::Notable);
        // 4K (notable) alongside Dolby Vision (best) in one HDR cell.
        assert_eq!(classify("DOLBY VISION"), SpecWeight::BestInClass);
    }

    /// §5's "data-driven, never hard-coded per row": every cell the strip
    /// builds carries the tier its own value implies, with no help from the
    /// slot it came from.
    #[test]
    fn fields_carry_their_weight() {
        let src = full_source();
        let fields = MediaFacts::from_source(&src).fields();
        let weight = |v: &str| {
            fields
                .iter()
                .find(|f| f.value == v)
                .map(|f| f.weight)
                .unwrap_or(SpecWeight::Baseline)
        };
        assert_eq!(weight("10-BIT"), SpecWeight::BestInClass);
        assert_eq!(weight("HEVC"), SpecWeight::Notable);
        assert_eq!(weight("HDR10"), SpecWeight::Notable);
        assert_eq!(weight("1080P"), SpecWeight::Baseline);
        assert_eq!(weight("MKV"), SpecWeight::Baseline);
    }

    /// Each tier must actually render in its own §5 colour -- a mapping
    /// collapse (two tiers resolving to one token) would silently undo the
    /// strip's entire information design without failing any other test.
    #[test]
    fn each_tier_maps_to_its_own_brand_colour() {
        assert_eq!(SpecWeight::Baseline.color(), theme::GRIGIO);
        assert_eq!(SpecWeight::Notable.color(), theme::PANNA);
        assert_eq!(SpecWeight::BestInClass.color(), theme::PISTACCHIO);
    }

    // ---- breakdown --------------------------------------------------

    #[test]
    fn breakdown_covers_every_stream_plus_the_file_section() {
        let src = full_source();
        let sections = MediaFacts::from_source(&src).breakdown();
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["VIDEO 1", "AUDIO 1", "SUBTITLE 1", "FILE"]);
        let video = &sections[0].rows;
        assert!(video.contains(&("PROFILE".into(), "MAIN 10 @ L5.1".into())));
        assert!(video.contains(&("FRAME RATE".into(), "23.976 FPS".into())));
        let audio = &sections[1].rows;
        assert!(audio.contains(&("SAMPLE RATE".into(), "48.0 KHZ".into())));
        assert!(audio.contains(&("CHANNELS".into(), "5.1 (6 CH)".into())));
        assert!(sections[2]
            .rows
            .contains(&("FLAGS".into(), "FORCED".into())));
        let file = &sections[3].rows;
        assert!(file.contains(&(
            "PATH".into(),
            "/media/Movies/Example (2019)/Example.mkv".into()
        )));
    }

    /// The strip's own eight-field width, so the fit tests below are
    /// anchored to a real number rather than a guessed one.
    fn full_width(trailing_info: bool) -> f32 {
        let src = full_source();
        strip_width_px(&MediaFacts::from_source(&src).fields(), trailing_info)
    }

    #[test]
    fn a_wide_container_yields_the_single_classic_row() {
        let src = full_source();
        let fields = MediaFacts::from_source(&src).fields();
        let rows = partition_fields(&fields, 2000.0, true);
        assert_eq!(rows, vec![fields]);
    }

    /// The width-unaware path (`spec_strip`) must stay lossless and flat.
    #[test]
    fn an_unknown_width_yields_one_row_with_everything() {
        let src = full_source();
        let fields = MediaFacts::from_source(&src).fields();
        assert_eq!(
            partition_fields(&fields, f32::INFINITY, false),
            vec![fields]
        );
    }

    /// Round 3's rule: narrowing the container may only ever add ROWS --
    /// never lose a value, never reorder one. (The old behaviour dropped
    /// SIZE/CONTAINER/BITRATE to fit; a narrow window lost
    /// `5.4 MBPS │ MKV │ 734.8 MB` to exactly that.)
    #[test]
    fn every_value_survives_every_width_in_order() {
        let src = full_source();
        let fields = MediaFacts::from_source(&src).fields();
        let full = full_width(false);
        let mut w = full;
        while w > 0.0 {
            let rows = partition_fields(&fields, w, false);
            let flattened: Vec<SpecField> = rows.iter().flat_map(|r| r.iter().cloned()).collect();
            assert_eq!(
                flattened, fields,
                "at avail={w}px the partition lost or reordered a value"
            );
            // Each row individually fits (except a lone over-wide field,
            // which is allowed to overflow rather than be cut).
            for row in &rows {
                assert!(
                    row.len() == 1 || strip_width_px(row, false) <= w,
                    "at avail={w}px a multi-field row overflows"
                );
            }
            w -= full / 7.0;
        }
    }

    /// An absurdly narrow container degenerates to one field per row --
    /// still nothing hidden.
    #[test]
    fn an_absurdly_narrow_container_gets_one_field_per_row() {
        let src = full_source();
        let fields = MediaFacts::from_source(&src).fields();
        let rows = partition_fields(&fields, 10.0, true);
        assert_eq!(rows.len(), fields.len());
        assert!(rows.iter().all(|r| r.len() == 1));
    }

    /// The reported orphan was `802.8 MB | ⓘ INFO` alone on a second line:
    /// the ⓘ cell's width has to be part of the last row's budget, or the
    /// strip "fits" and then the affordance pushes it over anyway. The
    /// anti-orphan reflow moves the last FIELD down instead of dropping it.
    #[test]
    fn the_info_affordance_is_reserved_on_the_last_row() {
        let src = full_source();
        let fields = MediaFacts::from_source(&src).fields();
        let avail = full_width(false); // fits exactly WITHOUT the ⓘ cell...
        assert_eq!(partition_fields(&fields, avail, false).len(), 1);
        // ...and must reflow (not drop) once the ⓘ cell is accounted for.
        let rows = partition_fields(&fields, avail, true);
        assert!(rows.len() > 1);
        let flattened: Vec<SpecField> = rows.iter().flat_map(|r| r.iter().cloned()).collect();
        assert_eq!(flattened, fields);
        let last = rows.last().expect("non-empty");
        assert!(last.len() == 1 || strip_width_px(last, true) <= avail);
    }

    /// The width model is only defensible because the strip is monospace:
    /// same character count => same width, whatever the characters are.
    #[test]
    fn width_is_purely_a_function_of_character_count() {
        let a = vec![SpecField::new("1080P", SpecKind::Resolution)];
        let b = vec![SpecField::new("WXYZQ", SpecKind::Resolution)];
        assert_eq!(strip_width_px(&a, false), strip_width_px(&b, false));
        assert!(strip_width_px(&a, true) > strip_width_px(&a, false));
    }

    #[test]
    fn an_empty_strip_has_no_width() {
        assert_eq!(strip_width_px(&[], false), 0.0);
    }

    /// §5's ` │ ` separators are part of the laid-out width now that they
    /// are real characters rather than 1px divs -- a width model that forgot
    /// them would under-estimate by three columns per field and put back the
    /// ragged wrap `fit_fields` exists to prevent.
    #[test]
    fn separators_and_the_pill_inset_are_part_of_the_width() {
        let one = vec![SpecField::new("1080P", SpecKind::Resolution)];
        let two = vec![
            SpecField::new("1080P", SpecKind::Resolution),
            SpecField::new("HEVC", SpecKind::VideoCodec),
        ];
        // Adding a 4-char cell costs its own 4 columns PLUS the separator's.
        let delta = strip_width_px(&two, false) - strip_width_px(&one, false);
        assert_eq!(delta, (4.0 + SEPARATOR_COLS) * MONO_ADVANCE);
        // And a single cell already carries the pill's inset on both sides.
        assert_eq!(
            strip_width_px(&one, false),
            5.0 * MONO_ADVANCE + PILL_PAD * 2.0
        );
    }

    #[test]
    fn breakdown_omits_rows_with_no_data() {
        let src = source(serde_json::json!({
            "MediaStreams": [{ "Type": "Video", "Codec": "h264" }]
        }));
        let sections = MediaFacts::from_source(&src).breakdown();
        assert_eq!(sections.len(), 1);
        assert_eq!(
            sections[0].rows,
            vec![("CODEC".to_string(), "H264".to_string())]
        );
    }
}
