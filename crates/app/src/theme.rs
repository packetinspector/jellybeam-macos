//! Design tokens for the **Jellybeam** brand (`docs/DESIGN-GUIDE.md Part A`; layout
//! roles follow `docs/DESIGN-GUIDE.md` Part B §1-§7).
//!
//! The palette is the brand brief's, verbatim: eight named colors, one
//! accent, every neutral warm (§2).
//!
//! | Token        | Hex       | Role |
//! |--------------|-----------|------|
//! | `NOTTE`      | `#14100D` | App background |
//! | `SURFACE`    | `#1D1814` | Cards, sidebar rows, spec pills |
//! | `HAIRLINE`   | `#322A22` | 1px borders and dividers |
//! | `PANNA`      | `#F7E9CE` | Primary text, wordmark "Jelly" |
//! | `PANNA_2`    | `#C9C0B2` | Secondary text |
//! | `GRIGIO`     | `#8C8478` | Tertiary text, disabled, mono labels |
//! | `PISTACCHIO` | `#A8CB6B` | The **only** accent |
//! | `SHEEN`      | `#C6DE9B` | Defined, currently unused in the UI |
//!
//! Three rules from §2/§6, enforced here rather than by the compiler:
//!
//! * **One accent** -- no second hue for status, hover, or emphasis; every
//!   such role resolves to `ACCENT`, and `WARNING`/`DANGER` are warm
//!   *neutrals*, not a second and third hue.
//! * **Every neutral is warm** (red-yellow bias) -- no cool greys.
//! * **No gradients, no glows, no coloured shadows** -- `shadow_focus()` is
//!   a crisp zero-blur ring; elevation shadows stay neutral black; artwork
//!   carries no shadow at all.
//!
//! Every color/size/spacing/radius/motion value lives here as a named
//! `pub const`/`pub fn`; no call site should write a raw hex literal or an
//! unnamed `px(n.)`. Rule: grep for `rgb(0x`/`rgba(0x` outside this file
//! before landing a change to any restyled screen -- a hit means a token
//! was skipped, unless it falls under the OSD exception below.
//!
//! **OSD-over-video exception**: an element with no opaque/near-opaque
//! surface behind it (live video, or the bottom bar's thin scrim) has no
//! honest `surface.*` token to reach for, so its literal alpha stays a
//! literal. Floating panels (info overlay, track picker) are not exempt and
//! still get full token treatment. A literal's *hue*, even inside the
//! exempt category, still gets tokenized via `tint()` if it matches a named
//! token -- only alpha is exempt, never hue.
//!
//! This GPUI version cannot express the brand brief's **tracking** (§3's
//! `-0.02em`/`-0.03em`): `gpui = "0.2.2"` has no letter-spacing primitive.
//! Everything else in §2, §3 and §5 lands as a token here.
//!
//! A handful of tokens below are unconsumed until later screen retrofits
//! land (e.g. `TEXT_CAPTION`, `SUCCESS`/`WARNING`); kept rather than deleted
//! since they'd just be re-added verbatim.
#![allow(dead_code)]

use std::time::Duration;

use gpui::{
    ease_in_out, ease_out_quint, linear, point, pulsating_between, px, Animation, BoxShadow, Edges,
    FontFeatures, Hsla, Pixels,
};

// B.1 Type scale --
// GPUI's named scale (`text_xs()`..`text_3xl()`) covers 12-30px directly at
// call sites; Display/Title sit above it and need the raw `.text_size(px(n.))`
// escape hatch, sourced from the constants below.

// Brand §3: three faces, three jobs --
// All seven faces are vendored under `crates/app/assets/fonts/` and registered with
// GPUI's text system at startup (`main.rs::register_brand_fonts`). Family
// names match each TTF's `name` table.

/// **Archivo** -- every interface surface: titles, body, buttons, labels.
/// Only 400/700 are real weights in this family; `MEDIUM`/`SEMIBOLD` resolve to the nearest of those.
pub(crate) const FONT_UI: &str = "Archivo";
/// **Martian Mono** -- spec strips, keyboard shortcuts, status labels.
/// Nothing else (§3). Both 400 and 700 are real weights of this family.
pub(crate) const FONT_MONO: &str = "Martian Mono";
/// **Bagel Fat One** -- the word "Jellybeam" only, never UI/sentence/heading
/// (§3/§6). Sanctioned only at the sidebar wordmark and Connect screen.
pub(crate) const FONT_DISPLAY: &str = "Bagel Fat One";

/// Display role: 34px/Bold (§3's "Screen title"). Detail/Home hero title.
pub(crate) const TEXT_DISPLAY: Pixels = px(34.);
/// Tight leading for the Display role (~1.1x, Apple large-title convention).
pub(crate) const TEXT_DISPLAY_LINE_HEIGHT: Pixels = px(44.);
/// Title role: 28px/Bold. Screen-level headers with no hero.
pub(crate) const TEXT_TITLE: Pixels = px(28.);
/// Body role: 15px/Regular (HIG Subheadline, chosen over Body/17pt, which reads large in dense desktop layouts).
pub(crate) const TEXT_BODY: Pixels = px(15.);
/// Metadata role: 13px/Regular (HIG Footnote, exact match).
pub(crate) const TEXT_METADATA: Pixels = px(13.);
/// Caption/badge role: 11px/Semibold (HIG Caption 2, weight bumped for legibility at badge scale).
pub(crate) const TEXT_CAPTION: Pixels = px(11.);
/// Spec strip / shortcut-status role (§3): Martian Mono 10px, not `TEXT_CAPTION` -- its wide 0.70em advance needs the smaller size to fit.
pub(crate) const TEXT_SPEC: Pixels = px(10.);
// Section role (20px/Semibold) is GPUI's `text_xl()`; Card title role
// (14px/Medium) is `text_sm()` -- both used directly with `.font_weight(..)`.

// `gpui = "0.2.2"` has no letter-spacing/tracking primitive. Display/Title
// tracking is approximated via `TEXT_DISPLAY_LINE_HEIGHT`'s tight leading
// plus Bold weight instead. `font_features` is real, though: `tabular_nums()` below.
/// OpenType `tnum`+`lnum` features -- tabular lining figures, so a
/// digit-swapping label (OSD/countdown) doesn't shift width. Apply via `apply_tabular_nums()` below.
pub(crate) fn tabular_nums() -> FontFeatures {
    FontFeatures(std::sync::Arc::new(vec![
        ("tnum".to_string(), 1),
        ("lnum".to_string(), 1),
    ]))
}

/// Fluent one-call equivalent of setting `tabular_nums()` via GPUI's
/// `.font(Font)` setter without having to respecify family/weight/style.
pub(crate) fn apply_tabular_nums<T: gpui::Styled>(mut el: T) -> T {
    el.text_style()
        .get_or_insert_with(Default::default)
        .font_features = Some(tabular_nums());
    el
}

// B.2 Color tokens --
// Surfaces are opaque solids (`rgb(SURFACE_*)`); text tokens carry their own
// alpha byte (`rgba(TEXT_*)`) per Apple's macOS label-color hierarchy (one
// base color -- PANNA, not white -- four opacities), since half these tokens
// paint over artwork, where a translucent value composites with the image.
// The ladder (100/70/45/25%) is guarded by the test at the bottom of this file.

// §2's eight named brand colors, verbatim -- every surface/text/accent
// token below is one of these, or one of these at an alpha.

/// `#14100D` -- the app background.
pub(crate) const NOTTE: u32 = 0x14100D;
/// `#1D1814` -- cards, sidebar rows, spec pills.
pub(crate) const SURFACE: u32 = 0x1D1814;
/// `#322A22` -- 1px borders and dividers.
pub(crate) const HAIRLINE: u32 = 0x322A22;
/// `#F7E9CE` -- primary text, the wordmark's "Jelly".
pub(crate) const PANNA: u32 = 0xF7E9CE;
/// `#C9C0B2` -- secondary text.
pub(crate) const PANNA_2: u32 = 0xC9C0B2;
/// `#8C8478` -- tertiary text, disabled, mono labels, baseline spec values.
pub(crate) const GRIGIO: u32 = 0x8C8478;
/// `#A8CB6B` -- **the** accent. See `ACCENT` below for the roles it covers.
pub(crate) const PISTACCHIO: u32 = 0xA8CB6B;
/// `#C6DE9B` -- defined, currently unused in the UI; lighten target for `ACCENT_HOVER`.
pub(crate) const SHEEN: u32 = 0xC6DE9B;

/// L0 -- app background, Home/Library page canvas. `NOTTE`.
pub(crate) const SURFACE_BASE: u32 = NOTTE;
/// L1 -- sidebar, cards, spec pills, empty-art fallback, shelf background. `SURFACE`.
pub(crate) const SURFACE_RAISED: u32 = SURFACE;
/// L2 -- Settings/Search/Menu panels, season-tab selected pill; one warm step above `SURFACE` (§2 names only three surface values).
pub(crate) const SURFACE_PANEL: u32 = 0x261F19;
/// L3 -- hover/selected row backgrounds, active tab; kept just below `HAIRLINE` in luminance so a bordered panel's border still reads lightest.
pub(crate) const SURFACE_OVERLAY: u32 = 0x312921;
/// Borders/dividers only, never a fill. `HAIRLINE`.
pub(crate) const SURFACE_HAIRLINE: u32 = HAIRLINE;

/// 100% PANNA -- titles, focused card captions.
pub(crate) const TEXT_PRIMARY: u32 = tint(PANNA, 0xff);
/// 70% PANNA -- body copy, unfocused card captions, list-row primary text (≈brief's `PANNA-2` over `NOTTE`).
pub(crate) const TEXT_SECONDARY: u32 = tint(PANNA, 0xb3);
/// 45% PANNA -- metadata lines (year/runtime/genre), section labels.
pub(crate) const TEXT_TERTIARY: u32 = tint(PANNA, 0x73);
/// 25% PANNA -- placeholder text, disabled labels, dim-context timestamps.
pub(crate) const TEXT_QUATERNARY: u32 = tint(PANNA, 0x40);
/// §5: Detail page's demoted codec/technical line (SD/H264/DTS-5.1 badges) -- 55% PANNA, a quiet footer outside the primary/secondary/tertiary/quaternary hierarchy.
pub(crate) const TEXT_TECHNICAL: u32 = tint(PANNA, 0x8c);
/// §6: episode grid 2-line synopsis -- 60% PANNA, same reasoning as `TEXT_TECHNICAL`.
pub(crate) const TEXT_SYNOPSIS: u32 = tint(PANNA, 0x99);
/// §5: spec strip's **baseline** field value -- `GRIGIO`, opaque (louder counterparts: `TEXT_SPEC_NOTABLE`/`TEXT_SPEC_BEST`).
pub(crate) const TEXT_SPEC_BASELINE: u32 = GRIGIO;
/// §5 tier 2, "values worth noticing": 4K, HEVC, AV1, HDR10, Direct Play, lossless audio.
pub(crate) const TEXT_SPEC_NOTABLE: u32 = PANNA;
/// §5 tier 3, "best in class": 10-bit, Dolby Vision, Atmos, DTS-HD, TrueHD --
/// the one place the accent appears as text rather than a fill.
pub(crate) const TEXT_SPEC_BEST: u32 = PISTACCHIO;
/// Spec strip's ` │ ` field separator glyph (§5: U+2502 with spaces). `GRIGIO` at 60%, quieter than a baseline value so separators recede behind the data.
pub(crate) const SPEC_SEPARATOR: u32 = tint(GRIGIO, 0x99);
/// §8: sidebar ⌘-number hints -- 40% PANNA, quieter than the prose ladder (trivia, read only when looked for).
pub(crate) const TEXT_HINT: u32 = tint(PANNA, 0x66);

/// §2: `PISTACCHIO`, the only accent in the product; every role that would
/// otherwise run its own hue resolves here (primary button fill, `PROGRESS`, `UNWATCHED_BADGE_BG`, `SUCCESS`, the focus ring, `TEXT_SPEC_BEST`).
pub(crate) const ACCENT: u32 = PISTACCHIO;
/// Hover: `ACCENT` lifted halfway toward `SHEEN` (§2's sanctioned lighter pistachio).
pub(crate) const ACCENT_HOVER: u32 = 0xB7D583;
/// Pressed: `ACCENT` darkened ~15% along the same hue.
pub(crate) const ACCENT_PRESSED: u32 = 0x8FAD5B;

// §2: `ACCENT` is indicator-only (progress fill, season-tab underline,
// sidebar active bar, focus ring, a toggle that is on); segmented controls and primary buttons
// use their own neutral tokens below, so the accent means exactly one
// thing: the single primary action (Play/Resume) per screen.

/// Selected chip/segment fill (bitrate/skip-segment/preset controls) -- PANNA
/// at 10%, NOT accent: "which option is chosen" differs from "press this."
pub(crate) const CONTROL_SELECTED_FILL: u32 = tint(PANNA, 0x1a);
/// Selected chip's hover fill -- one step brighter than `CONTROL_SELECTED_FILL`, same PANNA hue.
pub(crate) const CONTROL_SELECTED_FILL_HOVER: u32 = tint(PANNA, 0x26);
/// Selected chip's "1px inset top highlight." GPUI 0.2.2's `BoxShadow` has no
/// inset variant, so `chip_button` approximates it with a real 1px top border (`.border_t_1()`).
pub(crate) const CONTROL_SELECTED_TOP_HIGHLIGHT: u32 = tint(PANNA, 0x40);
/// Unselected chip/segment text -- 60% PANNA, quieter than `TEXT_SECONDARY` (70%) since the selected sibling carries the emphasis.
pub(crate) const TEXT_CONTROL_UNSELECTED: u32 = tint(PANNA, 0x99);

/// Toggle switch (`ui::components::toggle_switch`) track fill when off -- same value as `CONTROL_SELECTED_FILL`, named separately (different control).
pub(crate) const TOGGLE_TRACK_OFF: u32 = tint(PANNA, 0x1a);
/// Toggle track fill when on -- solid `ACCENT`, so an enabled setting reads at a glance (§2: on-state is an indicator).
pub(crate) const TOGGLE_TRACK_ON: u32 = tint(ACCENT, 0xff);
/// Toggle knob -- solid `PANNA`, always (on and off), so only the track color communicates state.
pub(crate) const TOGGLE_KNOB: u32 = PANNA;

/// §5 Primary button: `PISTACCHIO` fill, the accent's one large filled use.
pub(crate) const PRIMARY_BUTTON_BG: u32 = ACCENT;
/// Primary button hover -- `ACCENT_HOVER` (lifted toward `SHEEN`).
pub(crate) const PRIMARY_BUTTON_BG_HOVER: u32 = ACCENT_HOVER;
/// Primary button pressed -- `ACCENT_PRESSED` (darkened).
pub(crate) const PRIMARY_BUTTON_BG_PRESSED: u32 = ACCENT_PRESSED;
/// Primary button label -- §5's exact `#14100D` (`NOTTE`), not black: reads as the app background punched through the accent.
pub(crate) const PRIMARY_BUTTON_TEXT: u32 = NOTTE;

// §2 split color roles

/// Watched-progress fill -- `ACCENT` (§2's one-accent rule). Drawn
/// `PROGRESS_HEIGHT` tall, flush to the artwork's bottom edge, inside the image bounds, over `PROGRESS_TRACK`.
pub(crate) const PROGRESS: u32 = ACCENT;
/// The progress bar's track: same hue at 30% alpha, same bounds.
pub(crate) const PROGRESS_TRACK: u32 = tint(ACCENT, 0x4d);
/// §2's exact spec: 3px, flush-bottom, inside the artwork.
pub(crate) const PROGRESS_HEIGHT: Pixels = px(3.);

/// Unwatched indicator (series/season/box-set only): a filled numeric badge of
/// the unplayed count, never a naked dot. `PISTACCHIO` fill, `NOTTE` ink.
pub(crate) const UNWATCHED_BADGE_BG: u32 = ACCENT;
pub(crate) const UNWATCHED_BADGE_TEXT: u32 = NOTTE;
//
// No "unwatched" mark exists: an item with neither `cards.rs::watched_check_badge`
// nor the bottom `PROGRESS` bar reads as unwatched by omission, so an unwatched wall stays quiet.

// §3 focus/hover recipe -- border alone is banned. On hover/focus of any
// card, ALL of these fire together on `focus_enter_animation()`'s clock.

/// Card scale on hover/focus. Brand §5 caps this: "Hover lift no greater than `scale(1.02)`."
pub(crate) const FOCUS_SCALE: f32 = 1.02;
/// Card brightness multiplier on hover/focus (applied to the artwork).
pub(crate) const FOCUS_BRIGHTNESS: f32 = 1.08;
/// Opacity for the SIBLING cards in the row while one is focused.
pub(crate) const FOCUS_SIBLING_DIM: f32 = 0.5;
/// §8 icon button hover fill -- matches the OSD's icon buttons (`player_ui.rs`) so newly-restyled ones share this constant.
pub(crate) const ICON_HOVER_FILL: u32 = tint(PANNA, 0x22);

/// Fully transparent -- a placeholder border/gradient-stop color, named so
/// no call site spells `0x00000000` itself.
pub(crate) const TRANSPARENT: u32 = 0x00000000;

/// Direct Play / "connected" status dot -- §5's 6px `PISTACCHIO` dot, i.e. `ACCENT`, not a separate green.
pub(crate) const SUCCESS: u32 = ACCENT;
/// Offline/degraded status -- not a hue. §2 bans a second status colour, so
/// it's carried by wording, set in `GRIGIO`; distinguished from `DANGER` by loudness, not colour.
pub(crate) const WARNING: u32 = GRIGIO;
/// Error text, "Remove server" action -- also not a hue: `PANNA`, ordinary
/// primary text, since §5's error state is "one line, factual" -- the sentence is the signal, not a red.
pub(crate) const DANGER: u32 = PANNA;

/// `Hsla` conversions of the four text tokens, for call sites needing a typed color rather than raw `rgba(TEXT_*)`.
pub(crate) fn text_primary() -> Hsla {
    gpui::rgba(TEXT_PRIMARY).into()
}
pub(crate) fn text_secondary() -> Hsla {
    gpui::rgba(TEXT_SECONDARY).into()
}
pub(crate) fn text_tertiary() -> Hsla {
    gpui::rgba(TEXT_TERTIARY).into()
}
pub(crate) fn text_quaternary() -> Hsla {
    gpui::rgba(TEXT_QUATERNARY).into()
}

// B.3 Elevation & shadow --
// GPUI exposes a single-layer `BoxShadow`, not Material's umbra+penumbra+
// ambient stack. `spread_radius` stays `px(0.)` for every level but the focus
// ring. §10's four-level elevation scale (background luminance + shadow,
// used together for panels):
//
//   level     | bg               | shadow
//   ----------|------------------|--------------------------------
//   flat      | SURFACE_BASE     | none
//   raised    | SURFACE_RAISED   | shadow_e1  (0 4px 12px  35%)
//   floating  | SURFACE_PANEL    | shadow_e2  (0 12px 32px 60%)
//   overlay   | SURFACE_OVERLAY  | shadow_e3  (0 24px 60px 70%)  ← PiP, dialogs
//
// §2/§6 ban coloured shadows and glows, not shadows -- these stay neutral
// black. Artwork carries no shadow at all (§5); `cards.rs` animates scale/brightness only.

/// E1 "raised" -- cards at rest, shelf rows, list rows.
pub(crate) fn shadow_e1() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: gpui::rgba(0x00000059).into(),
        offset: point(px(0.), px(4.)),
        blur_radius: px(12.),
        spread_radius: px(0.),
    }]
}

/// E2 "floating" -- popovers, toasts, the Detail poster (§5's exact `0 12px 32px rgba(0,0,0,0.6)`).
pub(crate) fn shadow_e2() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: gpui::rgba(0x00000099).into(),
        offset: point(px(0.), px(12.)),
        blur_radius: px(32.),
        spread_radius: px(0.),
    }]
}

/// E3 "overlay" -- dialogs/sheets, the miniplayer (§7's exact `0 24px 60px rgba(0,0,0,0.7)`).
pub(crate) fn shadow_e3() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: gpui::rgba(0x000000b3).into(),
        offset: point(px(0.), px(24.)),
        blur_radius: px(60.),
        spread_radius: px(0.),
    }]
}

/// Keyboard focus ring width: 2px `PISTACCHIO`, zero blur (§2/§6 ban glows).
/// **Not the app's focus treatment** -- reach for `ui::components::focus_ring` instead.
pub(crate) const FOCUS_RING_WIDTH: Pixels = px(2.);
/// Gap between the focused element's edge and the ring -- CSS
/// `outline-offset: 2px` (a `BoxShadow` spread ring can't express a gap).
pub(crate) const FOCUS_RING_OFFSET: Pixels = px(2.);
/// `FOCUS_RING_OFFSET + FOCUS_RING_WIDTH` -- `focus_ring`'s negative inset.
pub(crate) const FOCUS_RING_OUTSET: Pixels = px(4.);
/// Clip-slack a horizontally-scrolling strip reserves around its cards so a
/// focused card's ring isn't cut by the strip's overflow clip.
pub(crate) const FOCUS_RING_CLEARANCE: Pixels = px(12.);
/// Vertical clip slack above the library grid/list's first row -- smaller
/// than `FOCUS_RING_CLEARANCE` since it overlaps only the toolbar's own `py_2` padding, never a control's hitbox.
pub(crate) const LIST_TOP_CLIP_SLACK: Pixels = px(8.);
pub(crate) fn shadow_focus() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: gpui::rgb(ACCENT).into(),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: FOCUS_RING_WIDTH,
    }]
}

// B.4 Spacing --
// Named roles on GPUI's existing 4px-step scale. Call sites should prefer
// the equivalent builtin (`.gap_4()`, `.p_3()`, ...); these exist for
// computed layout math needing a raw `Pixels`.

/// Icon-to-label gap inside a small button.
pub(crate) const SPACE_COMPACT: Pixels = px(4.);
/// Row internal gap, badge padding.
pub(crate) const SPACE_TIGHT: Pixels = px(8.);
/// List-row vertical padding, menu-item padding.
pub(crate) const SPACE_SNUG: Pixels = px(12.);
/// Card-to-card gutter, dialog body padding.
pub(crate) const SPACE_DEFAULT: Pixels = px(16.);
/// Shelf-to-shelf vertical gap.
pub(crate) const SPACE_COMFORTABLE: Pixels = px(20.);
/// Section-to-section gap within a page.
pub(crate) const SPACE_LOOSE: Pixels = px(24.);
/// Page horizontal margin.
pub(crate) const SPACE_SECTION: Pixels = px(32.);
/// Hero content inset from viewport edge.
pub(crate) const SPACE_PAGE: Pixels = px(40.);

// B.5 Radius --
// Alias of GPUI's own scale. Call sites should prefer the matching builtin
// (`.rounded_md()`, `.rounded_lg()`, ...); these exist for reference and the rare raw escape hatch.

/// Inputs and small chrome -- `.rounded_md()`. Not buttons (`RADIUS_PILL`) or cards/rows (`RADIUS_CARD`).
pub(crate) const RADIUS_CONTROL: Pixels = px(6.);
/// Menus/popovers, toasts -- `.rounded_lg()`.
pub(crate) const RADIUS_PANEL: Pixels = px(8.);
/// Dialogs/sheets -- `.rounded_xl()`.
pub(crate) const RADIUS_SHEET: Pixels = px(12.);
/// §5: cards, sidebar rows, list rows -- 8px radius, `SURFACE` fill, no border.
pub(crate) const RADIUS_CARD: Pixels = px(8.);
/// §5: poster art -- 2px radius, no shadow (every piece of artwork, distinct from its `RADIUS_CARD` container).
pub(crate) const RADIUS_ART: Pixels = px(2.);
/// §5's Detail poster -- artwork, so the same 2px as every other poster.
pub(crate) const RADIUS_POSTER: Pixels = RADIUS_ART;
/// §7's miniplayer window -- floating window chrome, not artwork, so it keeps the sheet radius.
pub(crate) const RADIUS_MINIPLAYER: Pixels = px(12.);
/// Badges, status pills, toggle switches -- `.rounded_full()`.
pub(crate) const RADIUS_PILL: Pixels = px(9999.);

// B.6 Motion --
// Durations from Material 3's published tokens; easing is the closest match
// from GPUI's built-in set. Each function returns a ready `Animation`.

/// Image fade-in, toast enter -- 150ms `ease_in_out`.
pub(crate) fn fade_animation() -> Animation {
    Animation::new(Duration::from_millis(150)).with_easing(ease_in_out)
}
/// Popover/menu open, card dwell-expand -- 200ms `ease_out_quint`.
pub(crate) fn reveal_animation() -> Animation {
    Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint())
}
/// Popover/menu close -- 150ms `ease_in_out` (snappier than open, by convention).
pub(crate) fn dismiss_animation() -> Animation {
    Animation::new(Duration::from_millis(150)).with_easing(ease_in_out)
}
/// Settings/Switcher sheet presentation, enter phase -- 250ms `ease_out_quint`.
pub(crate) fn sheet_enter_animation() -> Animation {
    Animation::new(Duration::from_millis(250)).with_easing(ease_out_quint())
}
/// OSD bar fade -- 150ms `ease_in_out`.
pub(crate) fn osd_animation() -> Animation {
    Animation::new(Duration::from_millis(150)).with_easing(ease_in_out)
}
/// Button press flash -- 100ms linear; most press feedback is an instant `.active()` style branch, not a tween.
pub(crate) fn micro_animation() -> Animation {
    Animation::new(Duration::from_millis(100)).with_easing(linear)
}

/// cubic-bezier(0.2, 0, 0, 1) — the §3 "emphasized decelerate" curve. Solved for y(x) via Newton iteration.
pub(crate) fn ease_emphasized() -> impl Fn(f32) -> f32 {
    |x: f32| {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        // x(t) with x1=0.2, x2=0.0 ; y(t) with y1=0.0, y2=1.0
        let sample_x = |t: f32| 3.0 * (1.0 - t) * (1.0 - t) * t * 0.2 + t * t * t;
        let sample_y = |t: f32| 3.0 * (1.0 - t) * t * t + t * t * t;
        let mut t = x;
        for _ in 0..8 {
            let dx = sample_x(t) - x;
            // derivative of sample_x
            let d = 3.0 * (1.0 - t) * (1.0 - t) * 0.2
                + 6.0 * (1.0 - t) * t * (-0.2)
                + 3.0 * t * t
                + 6.0 * (1.0 - t) * t * 0.0;
            if d.abs() < 1e-6 {
                break;
            }
            t = (t - dx / d).clamp(0.0, 1.0);
        }
        sample_y(t)
    }
}

/// §3 card focus/hover ENTER: 180ms, emphasized curve. All focus properties animate on this one clock.
pub(crate) fn focus_enter_animation() -> Animation {
    Animation::new(Duration::from_millis(180)).with_easing(ease_emphasized())
}
/// §3 card focus/hover EXIT: 240ms (slower out, per spec).
pub(crate) fn focus_exit_animation() -> Animation {
    Animation::new(Duration::from_millis(240)).with_easing(ease_emphasized())
}
/// §6 season-tab underline slide: 220ms.
pub(crate) fn tab_indicator_animation() -> Animation {
    Animation::new(Duration::from_millis(220)).with_easing(ease_emphasized())
}
/// §9 hero ambient-color crossfade: 400ms.
pub(crate) fn ambient_crossfade_animation() -> Animation {
    Animation::new(Duration::from_millis(400)).with_easing(ease_in_out)
}

/// §13 skeleton/loading pulse: opacity breathes `0.85`<->`1.0` over 2s, repeating; shape comes from `skeleton_opacity` below.
pub(crate) fn skeleton_animation() -> Animation {
    Animation::new(Duration::from_millis(2000))
        .repeat()
        .with_easing(linear)
}
/// Applies §13's `0.85..1.0` breathing range to a raw `0.0..1.0` animation delta.
pub(crate) fn skeleton_opacity(delta: f32) -> f32 {
    pulsating_between(0.85, 1.0)(delta)
}

// Misc layout constants shared by the popover/dialog components

/// Part B §9: never let a popover overflow the window edge.
pub(crate) fn popover_window_margin() -> Edges<Pixels> {
    Edges::all(px(16.))
}
/// §12: dialog/sheet scrim opacity, tinted `NOTTE` rather than pure black (a black wash over warm content reads as a grey cast, which §2 bans).
pub(crate) const SCRIM: u32 = tint(NOTTE, 0x8c);
/// Builds an `0xRRGGBBAA` value from a 24-bit `0xRRGGBB` color plus an 8-bit alpha.
pub(crate) const fn tint(hex_rgb: u32, alpha: u8) -> u32 {
    (hex_rgb << 8) | alpha as u32
}

/// Translucent-black-over-art overlay (e.g. `cards.rs::badges`' progress-track background) -- `NOTTE`-tinted rather than black, per §5's scrim spec.
pub(crate) const ART_SCRIM: u32 = tint(NOTTE, 0x80);

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the text-opacity tokens as strictly decreasing (§2's "one base color at decreasing opacity" hierarchy).
    #[test]
    fn text_opacity_tokens_strictly_decrease() {
        let alpha = |hex: u32| hex & 0xff;
        assert!(alpha(TEXT_PRIMARY) > alpha(TEXT_SECONDARY));
        assert!(alpha(TEXT_SECONDARY) > alpha(TEXT_TERTIARY));
        assert!(alpha(TEXT_TERTIARY) > alpha(TEXT_QUATERNARY));
    }

    /// Pins that the text ladder is one base hue (PANNA) at varying opacities, never a fresh grey per rung.
    #[test]
    fn text_tokens_are_all_panna_at_an_alpha() {
        for token in [
            TEXT_PRIMARY,
            TEXT_SECONDARY,
            TEXT_TERTIARY,
            TEXT_QUATERNARY,
            TEXT_TECHNICAL,
            TEXT_SYNOPSIS,
            TEXT_HINT,
        ] {
            assert_eq!(token >> 8, PANNA, "{token:#010x} is not PANNA at an alpha");
        }
    }

    /// Pins §2's "every neutral is warm": R > G > B for each surface/neutral token.
    #[test]
    fn every_neutral_is_warm() {
        let channels = |hex: u32| ((hex >> 16) & 0xff, (hex >> 8) & 0xff, hex & 0xff);
        for token in [
            NOTTE,
            SURFACE,
            HAIRLINE,
            PANNA,
            PANNA_2,
            GRIGIO,
            SURFACE_BASE,
            SURFACE_RAISED,
            SURFACE_PANEL,
            SURFACE_OVERLAY,
            SURFACE_HAIRLINE,
        ] {
            let (r, g, b) = channels(token);
            assert!(r > g && g > b, "{token:#08x} is not warm (R>G>B)");
        }
    }

    /// Pins that §2's surface ladder stays monotonically lighter (summed channels as a luminance proxy).
    #[test]
    fn the_surface_ladder_gets_lighter_at_every_step() {
        let sum = |hex: u32| ((hex >> 16) & 0xff) + ((hex >> 8) & 0xff) + (hex & 0xff);
        assert!(sum(SURFACE_BASE) < sum(SURFACE_RAISED));
        assert!(sum(SURFACE_RAISED) < sum(SURFACE_PANEL));
        assert!(sum(SURFACE_PANEL) < sum(SURFACE_OVERLAY));
        assert!(sum(SURFACE_OVERLAY) < sum(SURFACE_HAIRLINE));
    }

    /// Pins §2's one-accent rule: progress, the unwatched badge, connected/Direct-Play, the primary button, the focus ring, an on toggle and the spec strip's top tier all resolve to `PISTACCHIO`; status stays warm neutrals.
    #[test]
    fn there_is_exactly_one_accent() {
        for token in [
            ACCENT,
            PROGRESS,
            UNWATCHED_BADGE_BG,
            SUCCESS,
            PRIMARY_BUTTON_BG,
            TEXT_SPEC_BEST,
        ] {
            assert_eq!(token, PISTACCHIO, "{token:#08x} is a second accent");
        }
        assert_eq!(PROGRESS_TRACK >> 8, PISTACCHIO);
        assert_eq!(TOGGLE_TRACK_ON, (PISTACCHIO << 8) | 0xff);
        // Status carries no hue of its own.
        assert_eq!(WARNING, GRIGIO);
        assert_eq!(DANGER, PANNA);
    }

    /// Pins §2/§6's "no glows, no coloured shadows": the focus ring is the one coloured shadow, legal only because it's zero-blur.
    #[test]
    fn the_focus_ring_is_crisp_and_elevation_shadows_are_neutral() {
        let ring = shadow_focus();
        assert_eq!(ring[0].blur_radius, px(0.));
        assert_eq!(ring[0].spread_radius, FOCUS_RING_WIDTH);
        // `focus_ring` draws via fixed `.border_2()` (no `.border(Pixels)` in
        // 0.2.2), so this token and that call must stay in step.
        assert_eq!(FOCUS_RING_WIDTH, px(2.));
        assert_eq!(FOCUS_RING_OUTSET, FOCUS_RING_WIDTH + FOCUS_RING_OFFSET);
        for shadow in [shadow_e1(), shadow_e2(), shadow_e3()] {
            let c = shadow[0].color;
            assert_eq!(c.s, 0.0, "elevation shadows must be neutral, not tinted");
            assert_eq!(c.l, 0.0, "elevation shadows must be black");
        }
    }

    #[test]
    fn shadow_levels_grow_in_blur_radius() {
        let blur = |v: Vec<BoxShadow>| v[0].blur_radius;
        assert!(blur(shadow_e1()) < blur(shadow_e2()));
        assert!(blur(shadow_e2()) < blur(shadow_e3()));
    }
}
