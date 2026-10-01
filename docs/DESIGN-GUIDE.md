# Jellybeam — Brand and design guide

This guide covers everything visual in Jellybeam: the brand (Part A), what the pinned GPUI version can render (Part 0), the design system of tokens and shared components (Part B), the screens built from them (Part C), and which source files own each piece (Part D). Section identifiers are stable. Code comments cite this guide by section (`docs/DESIGN-GUIDE.md §A.4`, `B.9`, `C.5`), so a section keeps its number and subject once published; new material is appended as a new section rather than renumbering.

When Part A and Part B disagree, Part A wins: the brand fixes palette, faces and component shapes, and Part B fits the layout roles around them.

---

## Part A — Brand

### A.1 What the brand is

Jellybeam is a macOS Jellyfin client, written in Rust, native to Apple Silicon. Two things define it: it is polished where other clients are utilitarian, and it puts the technical detail of a file (resolution, codec, bit depth, HDR format, audio, bitrate, container) in front of you instead of hiding it.

Tone: quiet, precise, warm. State facts, never adjectives. No marketing language; the audience runs their own servers and will notice immediately.

- Primary tagline: **Your library, at native speed.**
- Personality tagline: **Small batch. Fast churn.**
- Descriptor: **A Jellyfin client for macOS.**
- Technical line: **Rust · Apple Silicon · Direct Play · mpv**

### A.2 Colour

| Token | Hex | Use |
|---|---|---|
| `NOTTE` | `#14100D` | App and window background |
| `SURFACE` | `#1D1814` | Cards, sidebar, spec pills |
| `HAIRLINE` | `#322A22` | 1px borders and dividers |
| `PANNA` | `#F7E9CE` | Primary text, wordmark "Jelly" |
| `PANNA-2` | `#C9C0B2` | Secondary text |
| `GRIGIO` | `#8C8478` | Tertiary text, disabled, mono labels |
| `PISTACCHIO` | `#A8CB6B` | The only accent: wordmark "beam", the update spinner's dot, best-in-class spec values, the app icon tile |
| `SHEEN` | `#C6DE9B` | Defined in `theme.rs`, unused in the UI |

Rules:

- One accent. Do not introduce a second hue for status, hover, or emphasis.
- Every neutral is **warm** (red-yellow bias). A neutral grey dropped in will look wrong.
- No gradients, glows, coloured shadows or translucency in UI chrome. The only gradients are legibility scrims over video and artwork and scroll-edge fades (Part 0).

### A.3 Type — three faces, three jobs

- **Bagel Fat One** — the word "Jellybeam" only. Never UI, never a sentence, never a heading.
- **Archivo** — every interface surface: titles, body, buttons, labels.
- **Martian Mono** — spec strips, keyboard shortcuts, status labels. Nothing else.

### A.4 The mark and the wordmark

**`jb_mascot_base.png`** is the product mark everywhere in the app UI: the sidebar header lockup and the About window, both through `root.rs::mascot_image` (`brand_lockup` wires it into the lockup). Every placement is the same PNG at a caller-chosen size, through `gpui::img()`'s default `ObjectFit::Contain`, so the source's real aspect ratio is always kept. Never tinted, recoloured, cropped, or rotated.

The macOS **app icon** is a separate, opaque asset: `icon-512.png`, a pistachio tile. It never appears inside the app. `scripts/make-icon.sh` builds `AppIcon.icns` from it with `sips` (scale) and `iconutil` (pack): every slot at or below 512×512 is a real downscale of the master; the 1024×1024 slot (the 512 entry's `@2x`) is the one upscale in the set (A.8).

**The wordmark**: "Jelly" in `PANNA`, "beam" in `PISTACCHIO`, Bagel Fat One, one baseline. One kerning adjustment, **−0.03em between the `y` and the `b`**, nothing else tracked or kerned. `root.rs::wordmark(size)` is the one implementation; the header lockup and the About window both call it.

GPUI 0.2.2's `TextRun` carries no per-run letter-spacing, so the kern is a negative left margin on a separate "beam" element rather than a true kerned glyph pair. If a later GPUI adds per-run tracking, `wordmark`'s two-`div` approximation can become a single `StyledText` run set and the margin goes.

Rays (a launch-screen flourish over the wordmark) are for marketing and launch screens only. This client has no launch screen distinct from the window, so it does not draw them.

**Header lockup**: mark beside wordmark, proportioned mark 35 / gap 6 / wordmark 20, scaled to this app's 22px wordmark. The mark overhangs its row rather than growing it: the row keeps a fixed footprint and the extra mark height paints above and below via negative margins, leaning toward the top in a 7:6 split. `root.rs`'s `LOCKUP_*` constants hold the exact figures.

### A.5 Components

**Spec strip** — the signature component. A 999px pill, `SURFACE` fill, 1px `HAIRLINE` border, Martian Mono 10–11px at −0.02em, values joined by ` │ ` (U+2502 with spaces). Three levels of emphasis, data-driven, never hard-coded per row:

| Level | Colour | When |
|-------|--------|------|
| baseline | `GRIGIO` | ordinary values — 1080p, H264, AC3, MKV, file size |
| notable | `PANNA` | values worth noticing — 4K, HEVC, Direct Play |
| best-in-class | `PISTACCHIO` | 10-bit, Dolby Vision, Atmos, DTS-HD |

Example: `1080P │ HEVC │ 10-BIT │ DTS 7.1 │ 21.4 MBPS │ MKV │ DIRECT PLAY`

**Buttons** — fully rounded, 999px. Primary: `PISTACCHIO` fill with a `#14100D` label, Archivo 700 15px, padding 11×22. Secondary: transparent with a 1px `HAIRLINE` border and `PANNA-2` label.

**Cards, sidebar rows, list rows** — 8px radius, `SURFACE` fill, no border. The selected nav row is a `SURFACE` fill only, with no left accent bar.

**Poster art** — 2px radius, no shadow. Hover lift no greater than `scale(1.02)`.

**Status** — a 6px `PISTACCHIO` dot plus a Martian Mono 10px label, e.g. `CONNECTED`.

**Hero / detail header** — backdrop image with a `NOTTE` scrim, then: Martian Mono `GRIGIO` breadcrumb, Archivo 700 34px title, buttons row, then the spec strip.

**Empty and error states** — Archivo, factual, one line, no illustration and no jokes: "No items in this library." "Server unreachable — check the Jellyfin URL in Settings." A handful of these states also carry a tier-2 mascot (A.7).

### A.6 Don't

- No second accent colour, no gradients in chrome, no glows, no coloured shadows.
- No neutral (cool) greys anywhere.
- Bagel Fat One never appears outside the word "Jellybeam".
- The mascot art is never the app icon, never tinted or recoloured, never repurposed as an animated UI control glyph.
- No emoji, no exclamation marks, no "blazing fast", no "just works".
- Never hide a spec value to make a row look tidier. The specs are the product.

### A.7 UI-state mascots (tier 2)

Used only on screens with **no content**, never over posters, backdrops, video, or a populated list. Always on `NOTTE` or `SURFACE`, at least 96px, `Contain` scaling, no tint, recolour or crop (A.4, A.6). Copy stays one factual line with no exclamation marks; the mascot carries the warmth, so the words don't need to.

| Pose | Where in this app |
|---|---|
| `jb_mascot_base` | Header lockup (sidebar and About window) — the product mark, not a tier-2 state (A.4) |
| `jb_mascot_curious` | Library page, genuinely empty (`raw_items.is_empty()`) |
| `jb_mascot_searching` | Search overlay and Discover search, no results |
| `jb_mascot_watching` | Quick Connect pairing screen |
| `jb_mascot_happy` | Not wired to a screen — no sign-in or connected confirmation exists |
| `jb_mascot_excited` | Not wired to a screen — no "new episodes" notice exists |
| `jb_mascot_fast` | Not wired to a screen — no launch screen distinct from the window exists |
| `jb_mascot_sleepy` | Not wired to a screen — no screensaver exists |

A library that is empty only because the active filters exclude everything is a different state (the library itself has items) and stays text-only, per the "no content" rule above.

### A.8 Known gaps

- **Icon upscale.** `AppIcon.icns`'s 1024×1024 slot is a `sips` upscale of the 512×512 master (A.4). There is no larger source icon yet.
- **Mascot resolution.** The mascot masters are small sources (roughly 360–640px). They read cleanly at every size the app uses (32–120px) but need a higher-resolution export before any larger placement (store art, print, marketing).
- **Poses without a screen.** `jb_mascot_happy`, `excited`, `fast` and `sleepy` have no matching UI state in this client (A.7). Wire one up only when that screen exists; don't force a placement.

---

## Part 0 — What GPUI 0.2.2 can and cannot do

Every value in this guide is renderable with the pinned `gpui = "0.2.2"` dependency, confirmed against the pinned crate source (`gpui-0.2.2` and `gpui-macros-0.2.2`) rather than the upstream docs, which have drifted. This section is the contract a spec must stay inside.

**Available:**

- **Solid fills**: `bg(rgb(...))` / `bg(rgba(...))`.
- **Linear gradients**: `gpui::linear_gradient(angle_deg, linear_color_stop(color, pct), ...)` (`src/color.rs`). Used for legibility scrims over video and artwork, and for scroll-edge fades.
- **Box shadow with blur**: `.shadow(vec![BoxShadow { color, offset, blur_radius, spread_radius }])`, a real blurred shadow. It is the only blur primitive available, and enough to build the elevation system (B.3).
- **Corner radius**: a fixed scale baked into the macro (`gpui-macros-0.2.2/src/styles.rs`): `rounded_none` 0px, `rounded_xs` 2px, `rounded_sm` 4px, `rounded_md` 6px, `rounded_lg` 8px, `rounded_xl` 12px, `rounded_2xl` 16px, `rounded_3xl` 24px, `rounded_full` 9999px, plus a raw `.rounded(px(n))` escape hatch. The radius tokens (B.5) map onto this scale; do not invent new radius values.
- **Interpolated animation**: `.with_animation(id, Animation::new(duration), |el, delta| el.opacity(delta))`, with `oneshot`/`repeat` and the built-in easings `linear`, `ease_in_out` (quadratic), `ease_out_quint()`, `bounce(f)` and `pulsating_between(min, max)` (`src/elements/animation.rs`). The closure runs over the element, so any style value computable from `delta: f32` is animatable (position, size, colour) as long as you write the lerp yourself. A custom easing is any `Fn(f32) -> f32`, which is how `theme.rs` supplies a cubic-bezier curve.
- **`anchored()` element**: positions a child relative to a `Corner` of a trigger point, with `.offset(point)` and either `.snap_to_window()` / `.snap_to_window_with_margin(edges)` (clamp to the viewport) or the default `SwitchAnchor` fit mode (flip to the opposite corner rather than overflow). The popover system (B.9) is built on it.
- **Font weights**: `FontWeight::{THIN 100, EXTRA_LIGHT 200, LIGHT 300, NORMAL 400, MEDIUM 500, SEMIBOLD 600, BOLD 700, EXTRA_BOLD 800, BLACK 900}`. A family resolves a missing weight to its nearest real one.
- **Text size**: named scale `text_xs`/`text_sm`/`text_base`/`text_lg`/ `text_xl`/`text_2xl`/`text_3xl` = 0.75/0.875/1.0/1.125/1.25/1.5/1.875 rem, with GPUI's rem = 16px, i.e. **12/14/16/18/20/24/30px**. Anything off that scale uses `.text_size(px(n.))`.
- **Spacing scale**: `p_*`/`m_*`/`gap_*` use the same 0.25rem-step scale: `_1` = 4px, `_2` = 8px, `_3` = 12px, `_4` = 16px, `_5` = 20px, `_6` = 24px, `_8` = 32px, `_10` = 40px, `_12` = 48px, `_16` = 64px, plus half-steps (`_0p5`, `_1p5`) for 2px/6px. GPUI already ships a consistent 4px grid; B.4 only names which steps to use where.
- **`deferred()` element**: paints its child after everything else in the frame regardless of tree position, with a priority for ordering between deferred elements (`src/elements/deferred.rs`). This is how a popover or toast stacks above sibling content without restructuring the tree.
- **OpenType features**: `FontFeatures` reaches the shaper, so tabular lining figures (`tnum`, `lnum`) are available for digit-swapping labels.

**Not available (design around these):**

- **No backdrop blur, frosted glass or vibrancy.** There is no `backdrop-filter` equivalent. Every surface is an opaque or semi-opaque flat fill; box-shadow blur blurs the shadow, not the backdrop.
- **No CSS-style transform (scale, rotate, skew) on arbitrary elements.** A "scale" is done by animating an element's actual width and height inside a fixed-size slot so siblings never move (B.3's card focus).
- **No image filters.** `img()` has no brightness or saturation filter; a brightness lift is a translucent overlay painted over the image.
- **No outline primitive.** A `border` paints inside the box, and a spread `BoxShadow` paints flush against it with no gap. An offset focus ring is an absolutely positioned sibling carrying its own border (B.3).
- **No letter-spacing.** Tracking values from the brand (A.3, A.4, A.5) cannot be expressed; the wordmark's single kern is a negative margin.
- **No native themed scrollbar.** `overflow_y_scroll()` / `overflow_x_scroll()` reserve OS-native scrollbar space, but the thumb and track colour cannot be restyled. The OS scrollbar is left as-is except where B.13 specifies a custom thumb.
- **No declarative CSS transitions.** Every animated change is an explicit `with_animation` closure, so B.6's motion tokens are duration and easing values passed into those calls, not a transition system.

---

## Part B — Design system

All tokens live in `crates/app/src/theme.rs` as `pub const` values or small `pub fn` builders. No screen file writes a raw hex literal or an unnamed `px(n.)`: grep for `rgb(0x` / `rgba(0x` outside `theme.rs` before landing a change to a screen. The one exception is the OSD over live video (C.6.1), where an element has no opaque surface behind it; there the literal alpha may stay a literal, but its hue still comes from a named token via `tint()`.

### B.1 Type scale

The numbers come from Apple's HIG SF Pro text-style ramp (Appendix). Where a cited size lands on GPUI's named scale the named call is used; otherwise the `.text_size(px(n.))` escape hatch carries the exact value. Faces follow A.3: Archivo for every role below, Martian Mono only for spec strips, shortcuts and status labels, Bagel Fat One only for the wordmark.

| Role | Size | Weight | GPUI call | HIG source | Use |
|---|---|---|---|---|---|
| Display | 34px | Bold (700) | `.text_size(theme::TEXT_DISPLAY)` | Large Title | Detail hero title, Home hero title |
| Title | 28px | Bold (700) | `.text_size(theme::TEXT_TITLE)` | Title 1 | Screen headers with no hero (Settings sheet, library and channel pages, shortcuts overlay) |
| Section | 20px | Semibold (600) | `.text_xl().font_weight(FontWeight::SEMIBOLD)` | Title 3 (exact) | Shelf titles, settings section headers |
| Card title | 14px | Medium (500) | `.text_sm().font_weight(FontWeight::MEDIUM)` | Below HIG's floor; a dense poster-wall caption | Poster and episode card captions |
| Body | 15px | Regular (400) | `.text_size(theme::TEXT_BODY)` | Subheadline (17pt Body reads large in dense desktop layouts) | Overview text, dialog body, list-row primary text |
| Metadata | 13px | Regular (400) | `.text_size(theme::TEXT_METADATA)` | Footnote (exact) | Year/runtime/genre lines, secondary row text |
| Caption / badge | 11px | Semibold (600) | `.text_size(theme::TEXT_CAPTION)` | Caption 2 (size exact, weight bumped for badge legibility) | Badge text, timestamp labels |
| Spec | 10px | Regular (400) | `.text_size(theme::TEXT_SPEC)` + `FONT_MONO` | Brand A.5 | Spec strips, status labels, sync pill |

Archivo has only 400 and 700 as real weights; Medium and Semibold resolve to the nearest of those. The three families are vendored under `crates/app/assets/fonts/` and registered at startup (`main.rs::register_brand_fonts`); the family-name constants are `theme::FONT_UI`, `FONT_MONO` and `FONT_DISPLAY`.

Line height: GPUI derives a default from font metrics. The Display role sets `.line_height(theme::TEXT_DISPLAY_LINE_HEIGHT)` (44px), tight leading after Apple's large-title convention. Labels whose digits change in place (OSD times, countdowns) apply `theme::apply_tabular_nums` so they don't shift width.

### B.2 Color tokens

Text hierarchy is one base colour at decreasing opacity, not independent greys. Surface elevation is a ladder of pre-mixed opaque solids, since GPUI has no live translucent overlay that composites cheaply over arbitrary content. Every value is a brand colour (A.2) or a brand colour at an alpha.

**Surfaces** (opaque):

| Token | Hex | Elevation | Use |
|---|---|---|---|
| `SURFACE_BASE` | `#14100D` (`NOTTE`) | L0 | App background, Home and Library canvas |
| `SURFACE_RAISED` | `#1D1814` (`SURFACE`) | L1 | Sidebar, cards, spec pills, empty-art fallback, Settings rail |
| `SURFACE_PANEL` | `#261F19` | L2 | Settings, Search and popover panels, toasts |
| `SURFACE_OVERLAY` | `#312921` | L3 | Hover and selected row backgrounds, active tab |
| `SURFACE_HAIRLINE` | `#322A22` (`HAIRLINE`) | — | Borders and dividers only, never a fill |

The ladder gets lighter at every step and stays warm (R > G > B); tests in `theme.rs` pin both.

**Text** (`PANNA` at four opacities, after macOS's label-colour hierarchy of roughly 100/70/45/25%):

| Token | Value | Use |
|---|---|---|
| `TEXT_PRIMARY` | `PANNA` 100% | Titles, focused captions |
| `TEXT_SECONDARY` | `PANNA` 70% (`0xb3`) | Body copy, unfocused captions, list-row text; close to `PANNA-2` over `NOTTE` |
| `TEXT_TERTIARY` | `PANNA` 45% (`0x73`) | Metadata lines, section labels, idle icons |
| `TEXT_QUATERNARY` | `PANNA` 25% (`0x40`) | Placeholders, disabled labels, dim timestamps |

A few named steps sit outside the four-rung ladder for specific jobs: `TEXT_TECHNICAL` (55%), `TEXT_SYNOPSIS` (60%, episode synopses), `TEXT_HINT` (40%, sidebar ⌘-number hints), `TEXT_CONTROL_UNSELECTED` (60%). All are `PANNA` at an alpha, pinned by a test.

**Accent and status:**

| Token | Value | Use |
|---|---|---|
| `ACCENT` | `PISTACCHIO` `#A8CB6B` | Primary button fill, progress fill, focus ring, season-tab underline, unwatched-count badge, best-in-class spec values, connected dot |
| `ACCENT_HOVER` | `#B7D583` | Primary button hover (halfway toward `SHEEN`) |
| `ACCENT_PRESSED` | `#8FAD5B` | Primary button pressed |
| `PRIMARY_BUTTON_TEXT` | `NOTTE` | Label on the accent fill |
| `SUCCESS` | `ACCENT` | Direct Play, connected status |
| `WARNING` | `GRIGIO` | Offline status — a warm neutral, not a hue |
| `DANGER` | `PANNA` | Error text, Remove actions — the sentence is the signal, not a red |

The accent means one thing per screen: the primary action, or state the eye should chase. Selected chips use neutral tokens instead (`CONTROL_SELECTED_FILL` = `PANNA` 10%). A toggle that is on is state the eye should chase, so its track is solid `ACCENT` (`TOGGLE_TRACK_ON`) under a solid `PANNA` knob. A test pins that every accent role resolves to `PISTACCHIO` and that status tokens carry no hue.

Scrims are `NOTTE`-tinted, not black: a black wash over warm content reads as a grey cast. `SCRIM` is `NOTTE` at 55% for dialogs; `ART_SCRIM` is `NOTTE` at 50% over artwork.

**Never** write a literal hex colour inline in a screen file.

### B.3 Elevation & shadow

GPUI exposes one shadow layer per element, not Material's umbra, penumbra and ambient stack. Elevation is background luminance and shadow used together. Shadows are neutral black; artwork carries no shadow at all (A.5).

| Level | Background | `BoxShadow` | Use |
|---|---|---|---|
| E0 flat | `SURFACE_BASE` | none | Page canvas, artwork |
| E1 raised | `SURFACE_RAISED` | `offset (0,4) blur 12 spread 0, rgba(0,0,0,35%)` | Cards, shelf rows, list rows |
| E2 floating | `SURFACE_PANEL` | `offset (0,12) blur 32 spread 0, rgba(0,0,0,60%)` | Popovers, toasts, the OSD info overlay |
| E3 overlay | `SURFACE_OVERLAY` / `SURFACE_PANEL` | `offset (0,24) blur 60 spread 0, rgba(0,0,0,70%)` | Dialogs and sheets, Search, the miniplayer |

Blur grows at every level; a test pins it.

**Focus.** The focus and current-item treatment is `ui::components::focus_ring`: a 2px `PISTACCHIO` stroke sitting 2px clear of the element's edge (CSS `outline: 2px solid; outline-offset: 2px`), zero blur. It is an absolutely positioned sibling at `-FOCUS_RING_OUTSET` (4px) with its own border, so the gap stays transparent and focus never shifts layout. It has no id and no listener, so it cannot swallow a click. The ring is fully enclosing and 2px thick, which meets WCAG 2.2's focus-appearance exception outright. Strips that clip their overflow reserve `FOCUS_RING_CLEARANCE` (12px) so a ring is never cut.

**Card focus and hover.** Everything fires together on one clock (`focus_enter_animation`, 180ms; `focus_exit_animation`, 240ms, both on the emphasized curve):

- The art box grows from 100% to `FOCUS_SCALE` (102%, the A.5 cap), centred in a fixed slot so siblings never move.
- A brightness lift of `FOCUS_BRIGHTNESS` (1.08×), painted as a white overlay.
- Sibling cards in the row dim to `FOCUS_SIBLING_DIM` (0.5).
- The focus ring, grown by the lift so it stays concentric.

Accent-coloured borders are for selection and focus only, never resting chrome. Panels define their edge with shadow, plus a `SURFACE_HAIRLINE` border on popovers.

### B.4 Spacing

Named roles on GPUI's 4px scale (Part 0). Call sites prefer the builtin (`.gap_4()`, `.p_3()`); the `theme::SPACE_*` constants exist for layout math that needs a raw `Pixels`.

| Token | GPUI step | px | Use |
|---|---|---|---|
| `SPACE_COMPACT` | `_1` | 4 | Icon-to-label gap inside a small button |
| `SPACE_TIGHT` | `_2` | 8 | Row internal gap, badge padding |
| `SPACE_SNUG` | `_3` | 12 | List-row vertical padding, menu-item padding |
| `SPACE_DEFAULT` | `_4` | 16 | Card-to-card gutter (`CELL_GAP`), dialog body padding |
| `SPACE_COMFORTABLE` | `_5` | 20 | Shelf-to-shelf vertical gap |
| `SPACE_LOOSE` | `_6` | 24 | Section-to-section gap within a page |
| `SPACE_SECTION` | `_8` | 32 | Page horizontal margin (`SCRUB_MARGIN`, Home, Library, Detail) |
| `SPACE_PAGE` | `_10` | 40 | Hero content inset from the viewport edge |

### B.5 Radius

Named by role, aliased onto GPUI's scale (Part 0):

| Token | GPUI class | px | Use |
|---|---|---|---|
| `RADIUS_ART` | `rounded_xs` | 2 | Every piece of artwork, including the Detail poster (A.5) |
| `RADIUS_CONTROL` | `rounded_md` | 6 | Inputs, popover item rows, small chrome |
| `RADIUS_CARD` | `rounded_lg` | 8 | Cards, sidebar rows, list rows (A.5) |
| `RADIUS_PANEL` | `rounded_lg` | 8 | Popovers, toasts, the OSD info overlay |
| `RADIUS_SHEET` | `rounded_xl` | 12 | Dialogs and sheets (Settings, Search); the miniplayer window |
| `RADIUS_PILL` | `rounded_full` | 9999 | Every button (A.5), chips, badges, status pills, toggle switches, spec strips |

### B.6 Motion

Durations come from Material 3's published tokens (short 50–200ms, medium 250–400ms), which sit inside the band Apple's HIG describes. Easing is the closest available shape: `ease_out_quint()` stands in for emphasized-decelerate on anything entering, `ease_in_out` for standard motion. The card focus curve is a true `cubic-bezier(0.2, 0, 0, 1)`, solved by Newton iteration in `theme::ease_emphasized`.

| Builder | Duration | Easing | Use |
|---|---|---|---|
| `micro_animation` | 100ms | `linear` | Press flash; most press feedback is an instant `.active()` style branch |
| `fade_animation` | 150ms | `ease_in_out` | Image fade-in, toast enter and exit |
| `reveal_animation` | 200ms | `ease_out_quint()` | Popover open |
| `dismiss_animation` | 150ms | `ease_in_out` | Popover close (snappier than open) |
| `sheet_enter_animation` | 250ms | `ease_out_quint()` | Settings sheet presentation |
| `osd_animation` | 150ms | `ease_in_out` | OSD bar fade |
| `focus_enter_animation` / `focus_exit_animation` | 180 / 240ms | emphasized | Card focus and hover (B.3) |
| `tab_indicator_animation` | 220ms | emphasized | Season-tab underline slide |
| `ambient_crossfade_animation` | 400ms | `ease_in_out` | Home hero crossfade |
| `skeleton_animation` | 2000ms, repeat | `linear` + `skeleton_opacity` (0.85–1.0) | Loading placeholder pulse |

Animate only what the user touched. A hover-triggered reveal waits on a dwell (`cards.rs::poster_card`'s `on_hover_dwell`, 350ms) so a pointer passing over a row does not set off a cascade.

### B.7 Iconography

Icons are Lucide SVGs (ISC), vendored unmodified at `crates/app/assets/icons/*.svg`: 24×24 viewBox, 2px stroke, `stroke="currentColor"`, loaded through `assets.rs`'s `AssetSource` and drawn with `svg().path("icons/<name>.svg")`. Add a new icon from the same source, the same way.

| Icon | File | Use |
|---|---|---|
| Arrow left / right | `arrow-left.svg`, `arrow-right.svg` | History back and forward |
| Chevron down | `chevron-down.svg` | Dropdown affordance on Sort and Filter |
| Chevron left / right | `chevron-left.svg`, `chevron-right.svg` | Disclosure, strip paging, breadcrumbs |
| Search | `search.svg` | Search trigger, Settings "Discover" section |
| Grid / List | `layout-grid.svg`, `list.svg` | Library view toggle; Home row; Settings "Home" and "Shortcuts" sections |
| Filter | `list-filter.svg` | Library filter button |
| Sort | `arrow-up-down.svg` | Library sort button |
| Check | `check.svg` | Selected row in popovers, active session |
| Server | `server.svg` | Switcher rows, Settings "Server & Account" |
| User | `circle-user.svg` | Account footer, server switcher trigger |
| Sliders | `sliders-horizontal.svg` | Settings "Playback" |
| Captions | `captions.svg` | OSD subtitle picker, Settings "Subtitles" |
| Info | `info.svg` | Settings "About" |
| Wifi-off | `wifi-off.svg` | Offline status |
| Triangle-alert | `triangle-alert.svg` | Error states |
| Plus | `plus.svg` | Add server |
| Trash | `trash-2.svg` | Remove server |
| Log-out | `log-out.svg` | Sign out |
| X | `x.svg` | Close and dismiss everywhere |

The OSD's own set (play, pause, skip, rotate, volume, audio-lines, maximize, minimize, picture-in-picture, settings) lives in the same directory.

**No Unicode glyph or emoji is ever a control affordance.** Close is `x.svg`, never `✕`; history is the arrow icons, never `←`/`→`; active and inactive server state is `ui::components::status_dot`, a styled div, never `●`/`○`; add is `plus.svg`, never `+`.

### B.8 Component: Buttons

`ui::components::button(id, label, variant, size, disabled)`. Every variant is a **999px pill** (A.5). Disabled is 40% opacity, no pointer cursor, no hover, and the caller attaches no `on_click`. Hover and press are instant style branches.

| Variant | Fill | Label | Border | Hover | Use |
|---|---|---|---|---|---|
| Primary | `ACCENT` | `NOTTE`, Archivo 700 | none | `ACCENT_HOVER`, pressed `ACCENT_PRESSED` | Play/Resume, Connect — one per screen |
| Secondary | transparent | `TEXT_SECONDARY`, Archivo 700 | 1px `SURFACE_HAIRLINE` | border lifts to `GRIGIO` (no fill, so it never reads as Primary) | More Info, Switch, Cancel |
| Ghost | transparent | `TEXT_SECONDARY`, `TEXT_PRIMARY` on hover | none | `SURFACE_OVERLAY` fill | Toolbar and tab labels |
| GhostDanger | transparent | `DANGER` | none | `SURFACE_OVERLAY` fill | Remove server |

Icon buttons are transparent with the icon in `TEXT_SECONDARY` or `TEXT_TERTIARY`, `ICON_HOVER_FILL` (`PANNA` at ~13%) on hover. History arrows are 28px; OSD controls keep their own sizing (`docs/DESIGN-PLAYER-NAV.md`).

| Size | Height | Label | Horizontal padding |
|---|---|---|---|
| `Sm` | 28px | 13px (Metadata) | 12px |
| `Md` | 36px | 15px (Body) | 16px |
| `Lg` | 44px | 15px Archivo 700 (A.5's 11×22 around a 15px label) | 22px |

Pill padding is wider than a rounded rect would need, to keep the label off the curve.

**Chips and toggles.** `chip_button` is a pill chip for segmented presets (bitrate cap, skip-segment action, subtitle preset), sized like `Sm`. Selected is `CONTROL_SELECTED_FILL` plus a 1px hairline ring; unselected is transparent at `TEXT_CONTROL_UNSELECTED`. Neither uses the accent. `toggle_switch` is a 36×20 track with a 16px `PANNA` knob; only the track colour carries state: `TOGGLE_TRACK_OFF` (`PANNA` 10%) when off, `TOGGLE_TRACK_ON` (solid `ACCENT`) when on.

### B.9 Component: Menus & Popovers

Every menu, picker and switcher is one shared, anchored popover (`crates/app/src/ui/popover.rs`). A popover opens next to what opened it; nothing contextual renders dead-centre in the window.

Shape:

```
popover_trigger(id, trigger, open, corner, offset, panel)
  div().relative()                      // container = trigger's own box
    .child(trigger)
    .when(open, deferred(               // paint above siblings, priority 1
        anchored()
          .anchor(corner)               // per-instance table below
          .offset(offset)               // typically point(0, 8): SPACE_TIGHT gap
          .snap_to_window_with_margin(Edges::all(px(16.)))
          .child(panel())))             // closure: a closed popover builds no rows
```

`anchored()` without `.position()` resolves against the relative container's on-screen box through the layout pass, so no manual viewport math is needed.

**`popover_panel` anatomy** (every instance):

- `SURFACE_PANEL` fill, `RADIUS_PANEL` (8px), `shadow_e2`, 1px `SURFACE_HAIRLINE` border, 4px inner padding.
- `min_w(200px)`, `max_w(360px)`: width-capped, never full-bleed.
- `max_h` per instance (400px by default) with `overflow_y_scroll()`: the panel never grows past its cap, whatever the item count.
- Opens with `reveal_animation` (200ms opacity fade).
- Stops its own click from reaching the catcher behind it.

**`popover_row`**: `RADIUS_CONTROL` (6px), 12px horizontal and 8px vertical padding. Hover and the keyboard cursor share one style, `SURFACE_OVERLAY`, so keyboard and mouse read the same. A selected row carries a leading or trailing `check.svg` in `TEXT_PRIMARY`.

**Click-away**: `click_away_catcher` is a full-window `deferred()` element at priority 0 with **no visible fill**, mounted from any full-size container while the panel (priority 1) is mounted deep at the trigger. Popovers are lightweight and contextual; Search and Settings are modal sheets with a scrim (B.12).

**Keyboard**: Up/Down move the highlight (`popover::move_highlight`, clamping like `focus_grid.rs`), Return activates, Esc closes.

**Per-instance anchors:**

| Trigger | Corner | Notes |
|---|---|---|
| OSD audio / subtitle buttons | `BottomRight` | Panel opens upward and leftward from the bottom bar |
| Library Sort / Filter buttons | `TopLeft`, offset 8px down | Panel hangs below the toolbar |
| Sidebar account footer (expanded and collapsed rail) | `BottomLeft` | Panel opens upward, since the footer sits at the bottom |
| Detail spec strip INFO | `TopLeft`, offset 8px down | Media breakdown, `max_h` 420px |
| Search overlay | — | Not a popover: a top-pinned modal (C.8) |
| Settings sheet | — | Not a popover: a modal sheet (B.12) |

### B.10 Component: Subtitle/Audio track picker

`player_ui.rs::render_track_picker_panel_from_parts`, built from B.9.

- Triggers: the OSD's `audio-lines.svg` and `captions.svg` buttons, anchored `BottomRight`.
- `max_h` 320px, tighter than the default because rows carry metadata; long track lists scroll inside the panel.
- The selected track has a leading `check.svg` in `TEXT_PRIMARY`; the row fill is the hover and keyboard highlight, so "selected" survives without hover.
- Row content: the track label, then `lang codec` secondary text in `TEXT_TERTIARY`, right-aligned and clipped at the panel edge.
- `popover::popover_section_label` exists for grouping rows (a Subtitles / Forced-SDH split); the picker does not group yet.

### B.11 Component: Server switcher

Full server management (add, remove, switch) lives in Settings → Server & Account. Quick switching is one click from a popover on the sidebar account footer: a swap between already-authenticated sessions with no re-login.

- Trigger: the sidebar account footer (`circle-user.svg` plus username), anchored `BottomLeft`; the collapsed icon rail has the same trigger.
- Panel (320px `max_h`): one row per stored session (`StoredSessionList`), `server.svg` (14px, `TEXT_TERTIARY`) leading, `username — host` label clipped to one line. The active session is `TEXT_PRIMARY` with a trailing `check.svg`; inactive rows are `TEXT_SECONDARY`.
- A click on an inactive row switches immediately. There is no Switch button inside the popover and no Remove action: a destructive action is never one accidental click away in a quick-access menu.
- Below a hairline divider, a "Settings" row opens the Settings sheet pre-navigated to Server & Account.

### B.12 Component: Dialogs & Sheets

`ui::components::dialog_scrim` and `dialog_panel`, used by Settings; Search applies the same recipe (C.8).

- **Scrim**: `SCRIM` (`NOTTE` at 55%), full viewport, centres its child. Heavy enough to focus attention, light enough that the browse content behind still reads as paused. `on_dismiss` is `Some` for a non-destructive sheet (click-away closes Settings) and `None` for a destructive confirm, which must be explicitly cancelled or confirmed.
- **Panel**: `SURFACE_PANEL`, `RADIUS_SHEET` (12px), `shadow_e3`, **no border**; the shadow alone defines the edge. Stops its own click from reaching the scrim. Entrance is `sheet_enter_animation` (250ms `ease_out_quint`).
- **Anatomy**: header row (16px padding, Title-role heading on the left, an `x.svg` icon-button close on the right), hairline divider, scrollable body with 16px padding. A footer row with right-aligned buttons is for confirm dialogs only; Settings has none, since its controls apply immediately.
- **Sizing**: Settings is 680px wide and one constant height, `min(640px, 80% of the window height)`, so switching sections never resizes the sheet and a short window never clips it. Tall sections scroll inside the body.

### B.13 Component: List rows, pills/badges, toasts, inputs, section headers, empty/loading states, scrollbars

- **List row** (`list_row`: sidebar rows, search results, server list, Settings rail): 8px radius, no border (A.5). Hover and selected are a `SURFACE_OVERLAY` fill. The selected row is a fill only, with no left accent bar (A.5): the accent is spent on actions, and a fill answers "which row am I on".
- **Pills and badges**: `RADIUS_PILL`, 8px horizontal / 2px vertical padding, Caption role. **Watched** is `cards.rs::watched_check_badge`, a check mark. **Unwatched** on a series, season or box set is a filled count badge (`UNWATCHED_BADGE_BG` accent, `NOTTE` ink), never a bare dot; a movie or episode with neither a check nor a progress bar reads as unwatched by omission. **Progress** is a 3px `PROGRESS` bar over a `PROGRESS_TRACK` (accent at 30%), flush to the artwork's bottom edge, inside the image. **Quality and codec** values are the spec strip (A.5), not loose badges.
- **Status pill**: `status_dot` (6px circle, filled `ACCENT` when active, `TEXT_TERTIARY` outline otherwise) plus `status_label` (Martian Mono 10px, uppercase, a warm neutral whose emphasis is carried by tone, not hue). Used by C.10.
- **Toast**: `toast_shell` — `SURFACE_PANEL`, `RADIUS_PANEL`, `shadow_e2`, top-centre. The caller fades it in and out with `fade_animation` under a stable per-toast id.
- **Text input** (`text_input.rs`): 34px tall, `SURFACE_RAISED` fill, 1px `SURFACE_HAIRLINE` border, `RADIUS_CONTROL`. Focus mounts the shared `focus_ring`, the same treatment as every other focusable element; the border stays 1px so focusing never resizes the box. Placeholder is `TEXT_QUATERNARY`.
- **Section headers**: two tiers. `section_title` is the Section role (20px semibold) for page-level groupings; `dense_label` is `TEXT_TERTIARY` at 13px for dense sub-groupings such as Settings labels. `form_row` and `form_row_desc` lay out Settings rows: label flush left, control cluster flush right, an optional wrapping description underneath.
- **Loading**: a placeholder tile (`cards.rs`) pulses with `skeleton_animation` (opacity 0.85 ↔ 1.0 over 2s, repeating) so a loading grid reads as loading rather than broken. No shimmer gradient.
- **Empty states**: `empty_state` is one factual Archivo line and nothing else (A.5, A.6). `empty_state_mascot` adds a 96px tier-2 mascot above the line, only on the screens A.7 names.
- **Text clamping**: `clamped_line` truncates to one line with a real "…" at a fixed reserved height; `clamped_block` clamps to N wrapped lines, measured against real font metrics, so every card or row in a group keeps the same height.
- **Edge fades**: a horizontally scrolling strip fades into the page colour at an overflowing edge (`STRIP_EDGE_FADE`, 48px), built as a gradient band because GPUI has no mask. It appears only when there is more to scroll to.
- **Scrollbars**: the OS-native scrollbar is left as-is. On macOS it auto-hides and is fine; GPUI cannot restyle it (Part 0).

---

## Part C — Screens

Each screen states its layout in px, the tokens it uses, and its states.

### C.1 Global chrome

- Window background: `SURFACE_BASE`.
- Sidebar: `SIDEBAR_WIDTH` = 240px (`gl_video.rs`), `SURFACE_RAISED`. It collapses to a narrow icon rail (same rows, handlers and ⌘-number hints, with tooltips) while the miniplayer is open. Rows are `list_row`s: selected is a `SURFACE_OVERLAY` fill with no accent bar. Hints use `TEXT_HINT`.
- Top of the sidebar: the header lockup (`brand_lockup`, A.4), mark beside wordmark.
- History back and forward: `arrow-left.svg` / `arrow-right.svg` icon buttons, 28px.
- Bottom of the sidebar, pinned with `mt_auto`: the sync pill (C.4), the status pill (C.10), then the account footer, which triggers the server switcher (B.11).

### C.2 Connect / Login screen

`root.rs::render_connect`: a vertically centred 360px column. Simple, fast, no clutter.

```
┌──────────────────────── viewport ────────────────────────┐
│                  (vertically centred)                    │
│                  ┌──────────────────┐                    │
│                  │  [mark] Jellybeam │ ← brand_lockup,    │
│                  │                  │   SPACE_LOOSE below │
│                  │  Server URL      │                    │
│                  │  [             ] │                    │
│                  │  Quick Connect ◯ │ ← toggle_switch     │
│                  │  [Username     ] │                    │
│                  │  [Password     ] │                    │
│                  │  (   Connect   ) │ ← Primary, Lg pill  │
│                  │  status line     │                    │
│                  └──────────────────┘                    │
│                  w: 360px                                │
└──────────────────────────────────────────────────────────┘
```

- Identity: the same mark-plus-wordmark lockup as the sidebar, `SPACE_LOOSE` above the form.
- Fields: B.13's text input.
- Quick Connect: `toggle_switch`. With it on, the pairing screen shows the `jb_mascot_watching` pose (A.7) above the code.
- Connect: B.8 Primary, `Lg`.
- While connecting, the button label reads "Connecting..." and the button is disabled. There is no spinner graphic; GPUI has no spinner primitive.
- Error and status text sits below the form in `DANGER` (A.5's one factual line).

### C.3 Miniplayer chrome

`player_ui.rs::render_miniplayer` is minimal by design: play/pause, a scrub strip, close. No pickers or info panels in a picture-in-picture window.

- Window: `RADIUS_MINIPLAYER` (12px), `shadow_e3` (drawn by `render_miniplayer_shadow`), snapped to a window corner.
- Controls reveal on hover over a translucent scrim; icon buttons use `ICON_HOVER_FILL`, icons `TEXT_PRIMARY`.
- Close is `x.svg`.
- The scrub strip is a thin accent fill; no buffered-range layer at this scale.

### C.4 Home

`home.rs::render`: an optional hero, then a vertical stack of shelves.

```
┌──────────────────────────── content pane ─────────────────────────────┐
│ ┌───────────────────────────────────────────────────────────────────┐ │
│ │  HERO (only when Continue Watching has an item)                   │ │
│ │  h: 42% of viewport (min 320px), full-bleed backdrop,             │ │
│ │  120px bottom gradient into the page                              │ │
│ │  eyebrow (series)                                                 │ │
│ │  Title (Display, 34px)                                            │ │
│ │  S2 E4 · Episode Title   (Metadata, TEXT_TERTIARY)                │ │
│ │  ( ▶ Resume )  ( More Info )                                      │ │
│ └───────────────────────────────────────────────────────────────────┘ │
│  Continue Watching                             ← Section role        │
│  [card][card][card][card][card] →                                    │
│  Next Up                                                             │
│  [card][card][card] →                                                │
│  Latest in Movies                                                    │
│  [card][card][card][card] →                                          │
└──────────────────────────────────────────────────────────────────────┘
```

- **Hero**: `hero_candidate` is `shelves[0].items[0]` when that shelf is Continue Watching, so the hero reuses existing data with no extra query. No in-progress item means no hero, and the page opens straight into shelves. The backdrop reuses Detail's fallback chain (`detail.rs::backdrop_source`). CTAs: Resume (B.8 Primary) and More Info (Secondary, opens Detail). The eyebrow and metadata never repeat the title (`hero_eyebrow_and_meta`). The hero is full-bleed, so the page padding applies to the shelves column below it.
- **Shelf order**: Continue Watching first, then Next Up, then one Latest row per library. Nothing is ever inserted above Continue Watching; users react badly when a promoted row displaces it, as Apple found when it put a promotional row above Up Next in its TV app.
- **Sync in progress**: while `Mirror::is_syncing()`, a slim pulsing "Syncing your library…" line sits in the page's top padding. It never blocks first paint; shelves render whatever has already landed. The sidebar's sync pill carries the determinate version: Martian Mono `SYNCING <LIBRARY> — n OF m` with a 3px accent progress bar, omitted while the total is still unknown.
- **Shelves**: Section-role titles, 16px card gap, 20px between shelves, `SPACE_SECTION` (32px) horizontal inset.

### C.5 Library grid

`root.rs` wraps the grid (`grid.rs`) or list (`library_list.rs`) with a title and a toolbar.

```
┌──────────────────────────── content pane ─────────────────────────────┐
│  Movies                                          ← Title role (28px)  │
│  (⇅ Sort: Name ⌄) (⩩ Filter ⌄)  1,204 items          [▦] [☰]        │
│  ← toolbar, SPACE_SECTION inset                                       │
│  [card][card][card][card][card][card]                                 │
│  [card][card][card][card][card][card]      ← virtualized uniform_list │
│  [card][card][card][card][card][card]                                 │
└──────────────────────────────────────────────────────────────────────┘
```

- **Toolbar** (`root.rs::library_toolbar`):
  - Sort: a 36px Secondary-style pill with `arrow-up-down.svg` leading and `chevron-down.svg` trailing, label `Sort: <key>`. It opens a B.9 popover with exactly `media_cache::Sort`'s options: Name, Date Added, Premiere.
  - Filter: the same shape with `list-filter.svg`. Its popover holds an Unwatched toggle and the library's genre list.
  - An item count in `TEXT_TERTIARY`.
  - Grid / List view toggle (`layout-grid.svg` / `list.svg`), pinned right with `ml_auto`; the active icon is `TEXT_PRIMARY`, the inactive one `TEXT_TERTIARY`.
- **Page title**: Title role (28px bold).
- **Grid**: `uniform_list` virtualization, `CELL_WIDTH` / `CELL_GAP` (160 / 16px). The 120fps poster-wall budget in `docs/OVERVIEW.md` §5b applies; changes here are measured against it.
- **Empty**: a genuinely empty library shows `jb_mascot_curious` with one line. A library emptied only by filters is text-only (A.7).

### C.6 Player OSD

The OSD's structure (gradient scrim, SVG icons, folded time labels, buffered range, centre flash, volume slider, chapter nav, never hiding while paused) is specified in `docs/DESIGN-PLAYER-NAV.md` Part 1. This section covers its tokens and layout rules.

**C.6.1 Tokens.** `ACCENT` is the played-progress fill. Floating panels (info overlay, track picker) get full token treatment. Elements with only video or the thin bottom scrim behind them keep literal alphas for track fills, since no opaque surface token honestly describes "over arbitrary video"; their hue still comes from a token via `tint()`.

**C.6.2 Title and info overlay.** The OSD title and the info overlay (`I`) are flex children of one `flex_col` anchored once at `top(16px)`, `left(SCRUB_MARGIN)` = 32px, with a `SPACE_SNUG` (12px) gap (`render_top_left_stack`). Flex layout stacks them, so they cannot collide whatever either one's height. The title follows the OSD's auto-hide; the info overlay stays up independently while toggled on.

```
┌─ top-left, y: 16px, x: 32px ───────────────────────────────┐
│  Series · S3 E7 · Episode Title          ← title            │
└──────────────────────────────────────────────────────────────┘
        ↓ 12px (SPACE_SNUG), when the info overlay is open
┌─ info overlay, 420px wide ───────────────────────────────────┐
│  Container         MKV                                       │
│  Playback          Direct Play                               │
│  ...                                                         │
└──────────────────────────────────────────────────────────────┘
```

**C.6.3 Info overlay.** `render_info_overlay`: 420px wide (fits the media-breakdown rows and a file path), `SURFACE_PANEL` chrome, `RADIUS_PANEL`, `shadow_e2`. Labels `TEXT_TERTIARY`, values `TEXT_PRIMARY`. Its `max_h` comes from the live viewport minus the OSD's bottom band; anything past it scrolls inside the panel.

**C.6.4 Everything else.** Bottom bar geometry, icon set, auto-hide rules, volume slider and chapter nav are as `docs/DESIGN-PLAYER-NAV.md` specifies. The track picker is B.10.

**C.6.5 Next-episode card.** `render_next_episode_card` has no container: its content sits on the video over a radial `NOTTE` scrim pinned to the frame's bottom-right corner, and rides up by the control zone only while the OSD shows. A depleting rule and an `IN n` numeral count down to autoplay. The 10s default delay is long enough to dismiss and short enough to keep a binge moving; it is a setting. Return or a click plays next; Esc dismisses. With autoplay off, the rule shows its bare track and the numeral is absent: the card never advances silently (pass-out protection, `docs/DESIGN-PLAYER-NAV.md` §2.1).

### C.7 Settings

`settings.rs::render`: a sidebar-plus-content sheet on B.12's scrim and panel. The section list stays visible at all times, so every section stays one click away.

```
┌──────────────────────── Settings (680 × min(640, 80vh)) ───────────────┐
│  Settings                                                       [x]    │ ← Title role
├────────────────┬───────────────────────────────────────────────────────┤
│ [srv] Server & │  ● user — host                    (Switch) Remove    │
│       Account  │  ○ user2 — host2                  (Switch) Remove    │
│ [sld] Playback │  (+ Add server)                                      │
│ [grd] Home     │                                                      │
│ [cc]  Subtitles│                                                      │
│ [srch] Discover│                                                      │
│ [lst] Shortcuts│                                                      │
│ [i]   About    │                                                      │
│  w: 190px      │  flex_1, own scroll                                  │
└────────────────┴───────────────────────────────────────────────────────┘
```

- **Rail**: 190px, `SURFACE_RAISED`, which separates it from the `SURFACE_PANEL` body without a border. Each section is a `section_row` (a `list_row`): a 16px icon (`ACCENT` when active, `TEXT_TERTIARY` otherwise) and a Body-role label (`TEXT_PRIMARY` active, `TEXT_SECONDARY` otherwise). Sections and icons: Server & Account (`server.svg`), Playback (`sliders-horizontal.svg`), Home (`layout-grid.svg`), Subtitles (`captions.svg`), Discover (`search.svg`), Shortcuts (`list.svg`), About (`info.svg`).
- **Body**: one `render_*_section` function per `SettingsSection`, built from `form_row` / `form_row_desc`, `chip_button` presets and `toggle_switch`es. Server rows use `status_dot` for active and inactive, with Switch and Remove buttons (Remove is `GhostDanger`). Shortcuts renders its bindings as `keycap` chips in Martian Mono.
- **Chrome**: B.12. No accent border.
- **Progressive disclosure**: every Playback control is always visible; no "Show Advanced" toggle. None of the current controls is expert enough to hide. The pattern is worth adopting once the section gains power-user controls such as a hardware-decode override or cache-size tuning.

### C.8 Search overlay

`search.rs`: a top-pinned modal with live filtering and a list of results.

- Scrim: `SCRIM`, the same as B.12; Search is functionally a modal. The panel sits 120px from the top, horizontally centred.
- Panel: 640px wide, `max_h` 520px, `SURFACE_PANEL`, `RADIUS_SHEET` (12px), `shadow_e3`, no border.
- Query header: the typed query at 18px over a divider; the placeholder is `TEXT_QUATERNARY`.
- Result rows: `list_row` — hover and selected are a `SURFACE_OVERLAY` fill.
- Empty: `jb_mascot_searching` above one factual line (A.7).
- Search stays deliberately minimal: one keyboard-handling path, no filter bar, no suggestion chips. A self-hosted library returns few enough results that a flat list scans fine.

### C.9 Series / Episode pages

`detail.rs`: backdrop, poster, metadata, seasons as tabs, episode grid, in a fixed order that answers "what is it, how good is the file, play it" top to bottom.

- Header (A.5): backdrop with a `NOTTE` scrim (`backdrop.rs`), a Martian Mono `GRIGIO` breadcrumb, the title in the Display role (34px, 44px leading), the buttons row, then the spec strip.
- Poster: `RADIUS_POSTER` (2px, like all artwork), no shadow (A.5).
- Play/Resume: B.8 Primary, `Lg`.
- Spec strip (`ui::spec_strip`): pill rows with three-level emphasis (A.5). An INFO control on the same row opens the full media breakdown in a B.9 popover (`TopLeft`, `max_h` 420px).
- Season tabs: the active tab has a 3px accent underline that slides between tabs on `tab_indicator_animation` (220ms).
- Episode cards: `cards.rs::episode_card`, sharing `poster_card`'s focus, badge and progress system; synopses clamp to two lines in `TEXT_SYNOPSIS`.
- Horizontal rails (seasons, cast, similar) fade at an overflowing edge (B.13).
- There is no separate episode detail page: an episode in the rail plays on click (`docs/DESIGN-PLAYER-NAV.md` §2.4).

### C.10 Offline / error affordances — persistent status pill

Connection state is a persistent pill in the sidebar, directly above the account footer (`root.rs`, `sidebar-status-pill`). It sits in the sidebar, so it never reflows the content pane. It is always present, and its content changes with state. A pill that stays visible when all is well is more honest than a banner that vanishes, since vanishing gives no positive confirmation that the app noticed a reconnect.

```
┌─ sidebar, above the account footer ─────────┐
│  ●  CONNECTED                                │ ← online: filled accent dot
└──────────────────────────────────────────────┘
┌─ same position, offline ─────────────────────┐
│  ○  OFFLINE — HOST FROM CACHE                │ ← hollow dot, warm neutral
└──────────────────────────────────────────────┘
┌─ same position, error ───────────────────────┐
│  ○  PLAYBACK ERROR — message            [x]  │ ← emphasized label, x.svg
└──────────────────────────────────────────────┘
```

- Shape: `status_dot` plus `status_label` (B.13), `RADIUS_PILL`, 8px / 4px padding. Every state is the same shape; wording and weight distinguish them, not colour (A.2).
- **Online**: filled 6px accent dot, `CONNECTED`.
- **Offline**: hollow dot, `OFFLINE — <HOST> FROM CACHE`, for as long as `state.offline` holds (set in `root.rs::on_bus_event`). Browsing continues from the mirror; play is disabled with a reason.
- **Error**: hollow dot, emphasized label `PLAYBACK ERROR — <message>`, and an `x.svg` dismiss that calls `Root::dismiss_error`. Error takes priority over offline rather than stacking a second pill; offline shows again once the error clears.
- **Play disabled while offline**: the reason goes through `state.error`, so it surfaces in the same error pill with no separate UI.

---

## Part D — Component map

All paths are under `crates/app/src/`.

### Theme tokens

`theme.rs`: every token in B.1–B.6 (type, colour, elevation, spacing, radius, motion) as a `pub const` or small `pub fn`, with tests pinning the one-accent, warm-neutral, text-ladder and shadow rules.

### Icons

`crates/app/assets/icons/*.svg` (B.7), served through `assets.rs`. Brand images (mark, mascots) sit under the repository-level `crates/app/assets/brand/jellybeam/`, `assets.rs`'s second embed root.

### Popover

`ui/popover.rs`: `popover_trigger`, `click_away_catcher`, `popover_panel`, `popover_row`, `popover_section_label`, `move_highlight` (B.9).

### Shared components

`ui/components.rs`: buttons, chips, toggles, keycaps, list and form rows, status dot and label, section headers, toast shell, empty states, dialog scrim and panel, focus ring, text clamping, strip edge fades (B.8, B.12, B.13). `ui/spec_strip.rs` owns the spec strip (A.5). `ui/motion.rs` is the About window's motion vocabulary.

### Cards, Home and Library chrome

`cards.rs` (poster and episode cards, focus and badges), `home.rs` (hero, shelves, sync line), `grid.rs` and `library_list.rs` (grid and list views), and `root.rs` (sidebar, lockup, wordmark, library toolbar) — C.1, C.4, C.5.

### Detail pages

`detail.rs` and `backdrop.rs` — C.9.

### Player OSD

`player_ui.rs`: OSD, top-left title and info stack, track picker, next-episode card, toasts — C.6, B.10.

### Settings, server switcher and status pill

`settings.rs` (the sheet, C.7) and `root.rs` (server switcher B.11, sync and status pills C.10).

### Search overlay

`search.rs` — C.8.

### Connect screen and miniplayer chrome

`root.rs::render_connect` (C.2) and `player_ui.rs::render_miniplayer` (C.3).

---

## Out of scope

- A **confirm step for removing a server**: Remove in Settings fires on click. The intended shape is an inline "Remove this server? Cancel / Remove" in the row, not a modal on a modal.
- A **draggable** custom scrollbar thumb.
- The **Subtitles / Forced-SDH grouping** in the track picker (B.10): `player::Track::forced` exists, the grouped panel does not.
- A **subtitle timing-offset slider** (per-track ±5s).
- An **A–Z jump index** on the library grid for large libraries; it needs scroll-to-letter plumbing on `GridScroll`.
- **Individually dismissible Next Up entries** on Home ("remove series from Next Up").
- **Merging Continue Watching and Next Up** into one row. They stay separate, fixed-order shelves so the order is predictable.
- **Searchable Settings**, worth adding once the section count grows further.
- A **user-configurable OSD toolbar**. The OSD button set is fixed (`docs/DESIGN-PLAYER-NAV.md` §1.14).
- A **card context menu** (right-click and overflow actions). The popover component (B.9) is the base to build it from.
- A **logo-over-backdrop title treatment**. It needs an `ImageKind::Logo` variant in `media-cache`, which does not exist.
- **Autoplaying preview clips** on hover and an **expanded hover card**; a card's caption stays visible at rest, since Jellyfin posters are inconsistent about carrying the title in the art.

---

## Appendix — Where the numbers come from

The citation base for Part B. Apple's HIG pages are JavaScript-rendered, so Apple figures are corroborated through a reference table of the HIG text styles and a design-system breakdown; Material Design 3 and WCAG figures come from `m3.material.io` and `w3.org`.

- **Type scale** (Apple HIG, SF Pro text styles): Large Title 34/Bold, Title 1 28/Bold, Title 2 22/Bold, Title 3 20/Semibold, Headline 17/Semibold, Body 17/Regular, Callout 16/Regular, Subheadline 15/Regular, Footnote 13/Regular, Caption 1 12/Regular, Caption 2 11/Regular. SF Text is used below 40pt and SF Display at or above, so Display is a distinct typographic register, not just the biggest size. B.1 maps these to roles.
- **Spacing**: Apple's Layout guidance uses an 8pt grid (8/16/24/32pt) and a 20pt standard macOS window content margin; Material uses the same base-8 logic with a 4pt minimum sub-increment. Both match GPUI's built-in scale (Part 0), so B.4 only names roles.
- **Dark elevation**: Material 3 raises a light overlay's opacity with elevation — Level 0 0%, Level 1 5%, Level 2 8%, Level 3 11%, Level 4 12%, Level 5 14% — over the dark base. Apple's Dark Mode guidance has the same idea as a base-versus-elevated two-tier background (elevated is brighter, used for sheets and menus). B.2's surface ladder follows this, in warm brand steps.
- **Text hierarchy opacity**: macOS's label-colour hierarchy resolves to roughly 100% / 70% / 45% / 25% for primary, secondary, tertiary and quaternary text on a dark surface. B.2 uses these alphas on `PANNA`.
- **Shadow per elevation**: classic Material recipes combine umbra, penumbra and ambient layers at 0.20 / 0.14 / 0.12 opacity, with blur and offset growing per level. GPUI has one shadow layer, so each level is a single shadow whose blur grows with elevation (B.3).
- **Motion**: Material 3 duration tokens — short1–4 50/100/150/200ms, medium1–4 250/300/350/400ms, long1–4 450/500/550/600ms — with easing curves standard `cubic-bezier(0.2,0,0,1)`, emphasized-decelerate `cubic-bezier(0.05,0.7,0.1,1)`, emphasized-accelerate `cubic-bezier(0.3,0,0.8,0.15)`. Apple's figures (about 100–150ms for hover, 250–350ms for sheet presentation) sit in the same short and medium bands. Apple also advises animating only what the user interacts with and delaying hover-triggered reveals so a passing pointer doesn't set them off.
- **Focus visible**: WCAG 2.2 SC 2.4.11 / 2.4.13 Focus Appearance — the indicator covers at least a 2 CSS px perimeter around the component (or 4px along its shortest side) with at least 3:1 contrast between focused and unfocused states, except that a ring at least 2px thick that fully encloses the component needs no separate adjacent-contrast check. B.3's ring meets the exception.
