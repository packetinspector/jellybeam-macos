//! The About window: a separate, fixed-size GPUI window (`settings.rs::
//! open_about_window` opens it; also reached from the native
//! `Jellybeam > About Jellybeam` menu item, see `menu.rs`).
//!
//! ## Layout
//!
//! Top to bottom, centered, `NOTTE` background: an `ABOUT` label, a hairline
//! rule, the static `jb_mascot_base` mark, the `Jellybeam` wordmark, a
//! tagline, a version/build line, a spec pill, a bold tagline, two buttons,
//! and a copyright footer. See `render` for the concrete layout.
//!
//! The mark is `crate::root::mascot_image`, the same static, untinted
//! placement used everywhere else in the app (DESIGN-GUIDE.md §A.4) --
//! no click handling, no rotation, no easter egg. `rendered_geometry_tests`
//! pins the panel's vertical spacing (titlebar rule -> mark -> wordmark)
//! with real post-layout bounds via the `probed`/`Probe` machinery below.
//!
//! ## Motion architecture: ONE clock
//!
//! `Render::render` calls `AboutView::wants_animation_frame` and re-arms
//! `window.request_animation_frame()` only while there's still something to
//! animate: the staggered content entrance, or the update-check spinner.
//! Every animated value is a **pure function of an elapsed `Duration`**,
//! keeping them unit-testable and in lockstep.
//!
//! Deliberately *not* `gpui::AnimationExt::with_animation`: it keys a start
//! `Instant` off the element's positional `GlobalElementId`, so a staggered
//! entrance silently restarts every animation whose sibling index moved --
//! see `ui/motion.rs`'s module doc.
//!
//! **No timer can outlive the window.** `request_animation_frame` lives on
//! the window's next-frame callback list, re-armed from inside the next
//! `render`; closing the window drops the view entity and with it the frame
//! loop (`closing_the_window_drops_the_view_and_with_it_the_frame_loop`).
//! Clipboard-feedback `cx.spawn` timers are each guarded by a monotonically bumped
//! generation and by `Entity::update` returning `Err` once the view is gone.
//!
//! ## Reduced motion
//!
//! Read once at window-open time, mirroring `option_speed_hold.rs`'s "read
//! a native AppKit signal once" shape. When set: the entrance mounts in its
//! final state immediately.

use std::time::{Duration, Instant};

use gpui::{
    div, point, prelude::*, px, rgb, rgba, size, Bounds, ClipboardItem, Context, FocusHandle,
    KeyDownEvent, Render, SharedString, Window, WindowBackgroundAppearance, WindowBounds,
    WindowOptions,
};

#[cfg(test)]
use gpui::Pixels;

use crate::theme;
use crate::ui::components::{button, ButtonSize, ButtonVariant};
use crate::ui::motion;
use crate::ui::spec_strip::spec_separator;

// Window shell

const WINDOW_W: f32 = 420.0;
const WINDOW_H: f32 = 600.0;

/// Fixed, non-resizable window with a transparent traffic-light titlebar --
/// same idiom `main.rs` uses so the `NOTTE` panel paints to the window's top
/// edge with the native traffic lights floating over it.
pub(crate) fn window_options() -> WindowOptions {
    let bounds = Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(WINDOW_W), px(WINDOW_H)),
    };
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_background: WindowBackgroundAppearance::Transparent,
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("About Jellybeam".into()),
            appears_transparent: true,
            ..Default::default()
        }),
        is_resizable: false,
        window_min_size: Some(size(px(WINDOW_W), px(WINDOW_H))),
        ..Default::default()
    }
}

/// Reads macOS's "Reduce motion" preference once at window-open time and
/// threads it through as a plain `bool` -- see the module doc's "Reduced
/// motion" section. Unlike some AppKit calls elsewhere, this `NSWorkspace`
/// method takes no `MainThreadMarker`, but is only ever called from the
/// main thread.
pub(crate) fn reduce_motion_enabled() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

// Panel geometry -- the vertical budget the mark lives inside.

/// The titlebar band's height, then the hairline rule under it. Matches
/// `root.rs::TRAFFIC_LIGHT_INSET`'s ~40px clearance; named separately since
/// it's this window's own band, not a shared constant.
const TITLEBAR_BAND_H: f32 = 40.0;
const TITLEBAR_RULE_H: f32 = 1.0;
/// Gap between the hairline rule and the mark's top edge.
const MARK_TOP_GAP: f32 = 40.0;
/// Gap between the mark's bottom edge and the wordmark's top edge.
const MARK_BOTTOM_GAP: f32 = 24.0;
/// The static mark's display height (DESIGN-GUIDE.md §A.4), sized here
/// to keep this window's prior visual weight.
const MASCOT_SIZE: f32 = 120.0;
const MASCOT_ASSET: &str = "brand/jellybeam/jb_mascot_base.png";

// Motion constants

/// Copy-button hold.
const COPIED_HOLD: Duration = Duration::from_millis(1800);
/// Staggered-reveal delays, keyed by the element each one gates. The
/// window itself (delay 0) fades and rises underneath them.
const DELAY_WORDMARK_MS: u64 = 100;
const DELAY_TAGLINE_MS: u64 = 160;
const DELAY_VERSION_MS: u64 = 200;
const DELAY_SPEC_MS: u64 = 260;
const DELAY_TAGLINE_BOLD_MS: u64 = 300;
const DELAY_BUTTONS_MS: u64 = 340;
const DELAY_FOOTER_MS: u64 = 400;
/// How far the window rises into place, and how far each staggered content
/// element rises behind it.
const PANEL_RISE_PX: f32 = 14.0;
const CONTENT_RISE_PX: f32 = 8.0;
/// The whole entrance is over once the last-delayed element finishes --
/// the single condition `wants_animation_frame` checks rather than seven
/// separate ones.
const ENTRANCE_TOTAL: Duration = Duration::from_millis(DELAY_FOOTER_MS + motion::ENTRANCE_MS);

// Rendered-bounds probes

/// The named slots `rendered_geometry_tests` reads post-layout bounds out
/// of. Exists in every build so production code names each probed element
/// inline; the *recording* is `cfg(test)`-only -- see `probed`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Probe {
    MarkBox,
    TitlebarRule,
    Wordmark,
}

#[cfg(not(test))]
#[inline]
fn probed(el: gpui::Div, _slot: Probe) -> gpui::Div {
    el
}

#[cfg(test)]
thread_local! {
    /// Per-thread, so `#[gpui::test]`s (which render on their own test
    /// thread) never see each other's bounds.
    static PROBE_SINK: std::cell::RefCell<std::collections::HashMap<Probe, Bounds<Pixels>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(test)]
fn record_bounds(slot: Probe, bounds: Bounds<Pixels>) {
    PROBE_SINK.with(|sink| sink.borrow_mut().insert(slot, bounds));
}

/// Attaches an invisible `canvas` spy that records `el`'s rendered
/// screen-space bounds under `slot`. `absolute().inset_0()` (not
/// `size_full()`, which mis-anchors -- see `detail.rs::
/// hero_band_geometry_tests`) so the spy's own bounds *are* the host
/// element's padding box.
#[cfg(test)]
fn probed(el: gpui::Div, slot: Probe) -> gpui::Div {
    el.child(
        gpui::canvas(
            move |bounds, _window, _cx| record_bounds(slot, bounds),
            |_bounds, _prepainted, _window, _cx| {},
        )
        .absolute()
        .inset_0(),
    )
}

// Pure geometry/motion helpers -- unit-tested at the bottom of this file.

/// One staggered element's entrance frame: `(opacity, rise_px)`, a pure
/// function of `(elapsed, delay)`. `rise_px` shrinks to zero, applied as
/// `top(px(rise))`, a paint-time offset that never reflows siblings.
fn entrance_frame(elapsed: Duration, delay_ms: u64, rise_px: f32) -> (f32, f32) {
    let since_delay = elapsed.saturating_sub(Duration::from_millis(delay_ms));
    let eased = motion::entrance_ease()(motion::progress(since_delay, motion::ENTRANCE_MS));
    (eased, rise_px * (1.0 - eased))
}

/// "Copy build info"'s clipboard payload, pure and unit-tested. `macos_
/// version` is `None` when unavailable, which just omits the line.
fn build_info_text(
    version: &str,
    build: &str,
    rustc_version: &str,
    arch: &str,
    mpv: &str,
    playback_mode: &str,
    macos_version: Option<&str>,
) -> String {
    let mut lines = vec![
        format!("Jellybeam {version} build {build}"),
        format!("rustc {rustc_version}"),
        format!("arch {arch}"),
        mpv.to_string(),
        format!("playback {playback_mode}"),
    ];
    if let Some(macos_version) = macos_version {
        lines.push(format!("macOS {macos_version}"));
    }
    lines.join("\n")
}

/// `CARGO_CFG_TARGET_ARCH` (forwarded by `build.rs` as `JELLYBEAM_TARGET_ARCH`)
/// relabeled for display -- "ARM64/X86_64."
fn arch_label(raw: &str) -> String {
    match raw {
        "aarch64" => "ARM64".to_string(),
        "x86_64" => "X86_64".to_string(),
        other => other.to_uppercase(),
    }
}

/// mpv's `mpv-version` property reformatted for the spec pill, or
/// `"MPV UNKNOWN"` when `player::Player::mpv_version` returned `None`.
fn mpv_label(raw: Option<&str>) -> String {
    match raw {
        Some(raw) => {
            let version = raw.trim().strip_prefix("mpv ").unwrap_or(raw.trim());
            format!("MPV {version}")
        }
        None => "MPV UNKNOWN".to_string(),
    }
}

/// `NSProcessInfo.processInfo.operatingSystemVersionString` -- one
/// Objective-C message send, no `MainThreadMarker` required (matches
/// `reduce_motion_enabled`'s note).
fn macos_version_string() -> String {
    objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersionString()
        .to_string()
}

/// The footer's `©` year -- `chrono::Utc::now()` rather than a build-time
/// constant, per `build.rs`'s own doc comment on why the copyright year is
/// deliberately NOT baked in at compile time.
fn current_year() -> i32 {
    use chrono::Datelike;
    chrono::Utc::now().year()
}

/// The footer's exact copyright text.
fn copyright_line(build_year: i32) -> String {
    format!("© {build_year} · NOT AFFILIATED WITH JELLYFIN")
}

// View state

pub(crate) struct AboutView {
    focus_handle: FocusHandle,
    /// The window's one clock origin: the entrance stagger is a pure
    /// function of `opened_at.elapsed()`.
    opened_at: Instant,
    reduce_motion: bool,
    version: SharedString,
    build: SharedString,
    rustc_version: SharedString,
    arch: SharedString,
    mpv: SharedString,
    copied: bool,
    copy_gen: u64,
}

impl AboutView {
    /// `reduce_motion`/`mpv_version` are read once by `Root::
    /// open_about_window` and threaded straight through -- see their own
    /// doc comments for why neither is a per-frame or per-open re-check.
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        reduce_motion: bool,
        mpv_version: Option<String>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);

        Self {
            focus_handle,
            opened_at: Instant::now(),
            reduce_motion,
            version: SharedString::from(env!("CARGO_PKG_VERSION")),
            build: SharedString::from(env!("JELLYBEAM_BUILD")),
            rustc_version: SharedString::from(env!("JELLYBEAM_RUSTC_VERSION")),
            arch: SharedString::from(arch_label(env!("JELLYBEAM_TARGET_ARCH"))),
            mpv: SharedString::from(mpv_label(mpv_version.as_deref())),
            copied: false,
            copy_gen: 0,
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Escape only; Cmd+W is `menu.rs`'s app-wide `CloseWindow` action --
        // handling it here too would close the window twice over.
        if event.keystroke.key.as_str() == "escape" && !event.keystroke.modifiers.platform {
            cx.stop_propagation();
            window.remove_window();
        }
    }

    /// The single "is there anything left to animate?" predicate -- the
    /// only thing that decides whether `render` re-arms the frame loop.
    fn wants_animation_frame(&self, now: Instant) -> bool {
        if self.reduce_motion {
            return false;
        }
        now.saturating_duration_since(self.opened_at) < ENTRANCE_TOTAL
    }

    fn copy_build_info(&mut self, cx: &mut Context<Self>) {
        let text = build_info_text(
            self.version.as_ref(),
            self.build.as_ref(),
            self.rustc_version.as_ref(),
            self.arch.as_ref(),
            self.mpv.as_ref(),
            "Direct Play",
            Some(&macos_version_string()),
        );
        cx.write_to_clipboard(ClipboardItem::new_string(text));

        let generation = self.copy_gen.wrapping_add(1);
        self.copy_gen = generation;
        self.copied = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_HOLD).await;
            let _ = this.update(cx, |view, cx| {
                if view.copy_gen == generation {
                    view.copied = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

// Render

impl Render for AboutView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ---- the one clock -------------------------------------------
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.opened_at);
        let reduce_motion = self.reduce_motion;
        let entrance_done = reduce_motion || elapsed >= ENTRANCE_TOTAL;

        let mark = probed(div(), Probe::MarkBox)
            .mb(px(MARK_BOTTOM_GAP))
            .child(crate::root::mascot_image(MASCOT_ASSET, px(MASCOT_SIZE)));

        // ---- entrance stagger ----------------------------------------
        //
        // Every element below is present in EVERY frame -- no `Option`, no
        // conditional child, since appearing one at a time is what shifted
        // sibling indices and reset the old `with_animation` clocks.
        let staggered = move |el: gpui::Div, delay_ms: u64| -> gpui::Div {
            if entrance_done {
                return el;
            }
            let (opacity, rise) = entrance_frame(elapsed, delay_ms, CONTENT_RISE_PX);
            el.opacity(opacity).top(px(rise))
        };

        let wordmark = staggered(
            probed(div(), Probe::Wordmark).child(crate::root::wordmark(px(64.))),
            DELAY_WORDMARK_MS,
        );

        let tagline = staggered(
            div()
                .mt_2()
                .font_family(theme::FONT_UI)
                .text_size(px(20.))
                .text_color(rgba(theme::TEXT_SECONDARY))
                .child("A Jellyfin client for macOS."),
            DELAY_TAGLINE_MS,
        );

        let version_line = staggered(
            div()
                .mt_3()
                .font_family(theme::FONT_MONO)
                .text_size(theme::TEXT_SPEC)
                .text_color(rgb(theme::GRIGIO))
                .child(format!("VERSION {} · BUILD {}", self.version, self.build)),
            DELAY_VERSION_MS,
        );

        let spec_pill = staggered(
            div().mt_3().child(render_spec_pill(
                self.rustc_version.as_ref(),
                self.arch.as_ref(),
                self.mpv.as_ref(),
            )),
            DELAY_SPEC_MS,
        );

        let tagline_bold = staggered(
            div()
                .mt_4()
                .font_family(theme::FONT_UI)
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::PANNA))
                .child("Small batch. Fast churn."),
            DELAY_TAGLINE_BOLD_MS,
        );

        let update_button = div()
            .id("about-update-button")
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .h(px(44.))
            .px(px(22.))
            .rounded(theme::RADIUS_PILL)
            .bg(rgb(theme::PRIMARY_BUTTON_BG))
            .text_color(rgb(theme::PRIMARY_BUTTON_TEXT))
            .font_weight(gpui::FontWeight::BOLD)
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::PRIMARY_BUTTON_BG_HOVER)))
            .active(|s| s.bg(rgb(theme::PRIMARY_BUTTON_BG_PRESSED)))
            .child("View releases")
            .on_click(|_event, _window, cx| {
                cx.open_url("https://github.com/packetinspector/jellybeam-macos/releases");
            });

        let copy_label: SharedString = if self.copied {
            "Copied".into()
        } else {
            "Copy build info".into()
        };
        let copy_button = button(
            "about-copy-button",
            copy_label,
            ButtonVariant::Secondary,
            ButtonSize::Lg,
            false,
        )
        .on_click(cx.listener(|this, _event, _window, cx| this.copy_build_info(cx)));

        let buttons = staggered(
            div()
                .mt_5()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(update_button)
                .child(copy_button),
            DELAY_BUTTONS_MS,
        );

        let footer = staggered(
            div()
                .mt_5()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .child(div().w(px(64.)).h(px(1.)).bg(rgb(theme::SURFACE_HAIRLINE)))
                .child(
                    div()
                        .font_family(theme::FONT_MONO)
                        .text_size(theme::TEXT_SPEC)
                        .text_color(rgb(theme::GRIGIO))
                        .child(copyright_line(current_year())),
                ),
            DELAY_FOOTER_MS,
        );

        let content = div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .px(theme::SPACE_LOOSE)
            .pt(px(MARK_TOP_GAP))
            .child(mark)
            .child(wordmark)
            .child(tagline)
            .child(version_line)
            .child(spec_pill)
            .child(tagline_bold)
            .child(buttons)
            .child(footer);

        let panel = div()
            .id("about-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::NOTTE))
            .child(
                // Titlebar band: "ABOUT" top-right, clear of the traffic
                // lights on the left.
                div()
                    .flex_none()
                    .h(px(TITLEBAR_BAND_H))
                    .flex()
                    .items_center()
                    .justify_end()
                    .px_4()
                    .child(
                        div()
                            .font_family(theme::FONT_MONO)
                            .text_size(theme::TEXT_SPEC)
                            .text_color(rgb(theme::GRIGIO))
                            .child("ABOUT"),
                    ),
            )
            .child(
                probed(div(), Probe::TitlebarRule)
                    .flex_none()
                    .h(px(TITLEBAR_RULE_H))
                    .bg(rgb(theme::SURFACE_HAIRLINE)),
            )
            .child(content);

        // The ONLY place this window asks for another frame -- see the
        // module doc's "No timer can outlive the window."
        if self.wants_animation_frame(now) {
            window.request_animation_frame();
        }

        if entrance_done {
            return panel.into_any_element();
        }
        // The window's own fade/rise, delay 0, as an absolutely positioned
        // box at the known fixed `WINDOW_W`x`WINDOW_H` so the rise is a
        // pure translate with no reflow.
        let (panel_opacity, panel_rise) = entrance_frame(elapsed, 0, PANEL_RISE_PX);
        div()
            .relative()
            .size_full()
            .child(
                panel
                    .absolute()
                    .left_0()
                    .top(px(panel_rise))
                    .w(px(WINDOW_W))
                    .h(px(WINDOW_H))
                    .opacity(panel_opacity),
            )
            .into_any_element()
    }
}

/// The spec pill: `ui::spec_strip`'s visual language, but not its
/// `classify` machinery, which would misclassify "DIRECT PLAY" as merely
/// `Notable`. Each cell's color is stated directly here instead.
fn render_spec_pill(rustc_version: &str, arch: &str, mpv: &str) -> impl IntoElement {
    let cell = |text: String, color: u32| div().text_color(rgb(color)).child(text);
    let mut pill = div();
    pill.style().align_self = Some(gpui::AlignSelf::Center);
    pill.flex()
        .flex_row()
        .items_center()
        .font_family(theme::FONT_MONO)
        .text_size(theme::TEXT_SPEC)
        .px(px(12.))
        .py(theme::SPACE_COMPACT)
        .rounded(theme::RADIUS_PILL)
        .bg(rgb(theme::SURFACE_RAISED))
        .border_1()
        .border_color(rgb(theme::SURFACE_HAIRLINE))
        .child(cell(format!("RUST {rustc_version}"), theme::GRIGIO))
        .child(spec_separator())
        .child(cell(arch.to_string(), theme::PANNA))
        .child(spec_separator())
        .child(cell(mpv.to_string(), theme::GRIGIO))
        .child(spec_separator())
        .child(cell("DIRECT PLAY".to_string(), theme::PISTACCHIO))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- entrance (the pure function the whole stagger is built on) ----

    /// Pins the stagger's contract at four points: invisible/risen before
    /// its delay, mid-flight, and exactly landed at delay+duration and after.
    #[test]
    fn entrance_frame_is_pinned_at_its_delay_and_at_its_end() {
        let d = DELAY_SPEC_MS;
        let at = |ms: u64| entrance_frame(Duration::from_millis(ms), d, CONTENT_RISE_PX);

        let (opacity, rise) = at(0);
        assert_eq!(opacity, 0.0, "nothing before the delay");
        assert!((rise - CONTENT_RISE_PX).abs() < 1e-4);

        let (opacity, rise) = at(d);
        assert_eq!(opacity, 0.0, "the delay boundary is still the start");
        assert!((rise - CONTENT_RISE_PX).abs() < 1e-4);

        let (opacity, rise) = at(d + motion::ENTRANCE_MS / 2);
        assert!(
            opacity > 0.0 && opacity < 1.0,
            "mid-flight opacity out of range: {opacity}"
        );
        assert!(
            rise > 0.0 && rise < CONTENT_RISE_PX,
            "mid-flight rise {rise}"
        );

        let (opacity, rise) = at(d + motion::ENTRANCE_MS);
        assert!((opacity - 1.0).abs() < 1e-4);
        assert!(rise.abs() < 1e-4, "must land exactly, got {rise}");

        let (opacity, rise) = at(d + motion::ENTRANCE_MS + 10_000);
        assert!((opacity - 1.0).abs() < 1e-4, "and stay landed");
        assert!(rise.abs() < 1e-4);
    }

    /// Pins that the delays strictly increase top to bottom and finish by
    /// `ENTRANCE_TOTAL`, the bound `wants_animation_frame` uses.
    #[test]
    fn the_stagger_ladder_is_ordered_and_bounded() {
        let ladder = [
            DELAY_WORDMARK_MS,
            DELAY_TAGLINE_MS,
            DELAY_VERSION_MS,
            DELAY_SPEC_MS,
            DELAY_TAGLINE_BOLD_MS,
            DELAY_BUTTONS_MS,
            DELAY_FOOTER_MS,
        ];
        for pair in ladder.windows(2) {
            assert!(pair[0] < pair[1], "stagger ladder out of order: {ladder:?}");
        }
        let last = *ladder.last().expect("the ladder is a non-empty literal");
        assert_eq!(
            ENTRANCE_TOTAL,
            Duration::from_millis(last + motion::ENTRANCE_MS)
        );
        for delay in ladder {
            let (opacity, _) = entrance_frame(ENTRANCE_TOTAL, delay, CONTENT_RISE_PX);
            assert!(
                (opacity - 1.0).abs() < 1e-4,
                "delay {delay} is still animating at ENTRANCE_TOTAL"
            );
        }
    }

    // ---- footer / arch / mpv / build-info formatting --------------------

    #[test]
    fn copyright_line_matches_the_spec_format() {
        assert_eq!(
            copyright_line(2026),
            "© 2026 · NOT AFFILIATED WITH JELLYFIN"
        );
    }

    #[test]
    fn arch_label_relabels_known_targets() {
        assert_eq!(arch_label("aarch64"), "ARM64");
        assert_eq!(arch_label("x86_64"), "X86_64");
        assert_eq!(arch_label("riscv64"), "RISCV64");
    }

    #[test]
    fn mpv_label_strips_the_redundant_mpv_prefix() {
        assert_eq!(mpv_label(Some("mpv 0.38.0")), "MPV 0.38.0");
        assert_eq!(mpv_label(Some("0.38.0")), "MPV 0.38.0");
        assert_eq!(mpv_label(None), "MPV UNKNOWN");
    }

    #[test]
    fn build_info_text_matches_the_spec_format() {
        let text = build_info_text(
            "0.1.0",
            "1234",
            "1.82.0",
            "ARM64",
            "MPV 0.38.0",
            "Direct Play",
            Some("Version 15.0 (Build 24A335)"),
        );
        assert_eq!(
            text,
            "Jellybeam 0.1.0 build 1234\n\
             rustc 1.82.0\n\
             arch ARM64\n\
             MPV 0.38.0\n\
             playback Direct Play\n\
             macOS Version 15.0 (Build 24A335)"
        );
    }

    #[test]
    fn build_info_text_omits_macos_line_when_unavailable() {
        let text = build_info_text(
            "0.1.0",
            "1234",
            "1.82.0",
            "ARM64",
            "MPV 0.38.0",
            "Direct Play",
            None,
        );
        assert!(!text.contains("macOS"));
        assert_eq!(text.lines().count(), 5);
    }
}

/// The acceptance criteria for the panel's layout, measured rather than
/// argued: mounts a real `AboutView` and reads back post-layout bounds
/// through `canvas` spies (`probed`).
#[cfg(test)]
mod rendered_geometry_tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    /// Mounts an About window with reduced motion (deterministic, already
    /// settled bounds) and returns the probe sink's contents.
    fn render_and_probe(
        cx: &mut TestAppContext,
    ) -> std::collections::HashMap<Probe, Bounds<Pixels>> {
        PROBE_SINK.with(|sink| sink.borrow_mut().clear());
        let window = cx.add_window(|window, cx| {
            AboutView::new(window, cx, true, Some("mpv 0.38.0".to_string()))
        });
        // Parking the executor produces the laid-out frame these probes
        // read back (a dirty window renders for real under `cfg(test)`).
        let visual = VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        PROBE_SINK.with(|sink| sink.borrow().clone())
    }

    fn get(
        probes: &std::collections::HashMap<Probe, Bounds<Pixels>>,
        slot: Probe,
    ) -> Bounds<Pixels> {
        *probes
            .get(&slot)
            .unwrap_or_else(|| panic!("{slot:?} was never laid out -- the probe never fired"))
    }

    /// Pins that the mark sits between the titlebar and the wordmark, with
    /// the spacing the panel constants promise.
    #[gpui::test]
    fn the_mark_sits_between_the_titlebar_and_the_wordmark(cx: &mut TestAppContext) {
        let probes = render_and_probe(cx);
        let rule = get(&probes, Probe::TitlebarRule);
        let mark = get(&probes, Probe::MarkBox);
        let wordmark = get(&probes, Probe::Wordmark);

        let rule_bottom = f32::from(rule.origin.y) + f32::from(rule.size.height);
        let mark_top = f32::from(mark.origin.y);
        let mark_bottom = mark_top + f32::from(mark.size.height);
        let wordmark_top = f32::from(wordmark.origin.y);

        assert!(
            mark_top > rule_bottom,
            "the mark (top {mark_top}) must start below the titlebar rule \
             (bottom {rule_bottom})"
        );
        assert!(
            (mark_top - rule_bottom - MARK_TOP_GAP).abs() < 1.0,
            "expected ~{MARK_TOP_GAP}px between the titlebar rule and the mark, \
             got {}",
            mark_top - rule_bottom
        );
        assert!(
            mark_bottom < wordmark_top,
            "the mark (bottom {mark_bottom}) must end above the wordmark \
             (top {wordmark_top})"
        );
        assert!(
            (wordmark_top - mark_bottom - MARK_BOTTOM_GAP).abs() < 1.0,
            "expected ~{MARK_BOTTOM_GAP}px between the mark and the wordmark, \
             got {}",
            wordmark_top - mark_bottom
        );
    }

    /// Pins that a freshly-opened window holds the wordmark `CONTENT_RISE_PX`
    /// lower relative to the (static, non-reflowing) mark than a settled one
    /// does.
    #[gpui::test]
    fn the_entrance_offsets_the_wordmark_without_reflowing_the_mark(cx: &mut TestAppContext) {
        let gap = |probes: &std::collections::HashMap<Probe, Bounds<Pixels>>| {
            let mark = get(probes, Probe::MarkBox);
            let wordmark = get(probes, Probe::Wordmark);
            f32::from(wordmark.origin.y) - (f32::from(mark.origin.y) + f32::from(mark.size.height))
        };

        let settled = gap(&render_and_probe(cx));

        // Same window, entrance live: `AboutView::new` reads reduced motion
        // from its argument, so build this one with it off.
        PROBE_SINK.with(|sink| sink.borrow_mut().clear());
        let window = cx.add_window(|window, cx| AboutView::new(window, cx, false, None));
        let visual = VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        let entering = gap(&PROBE_SINK.with(|sink| sink.borrow().clone()));

        assert!(
            (settled - MARK_BOTTOM_GAP).abs() < 1.0,
            "settled gap should be the flat {MARK_BOTTOM_GAP}px, got {settled}"
        );
        assert!(
            entering > settled + 3.0,
            "mid-entrance the wordmark should still be riding {CONTENT_RISE_PX}px low \
             (gap {entering} vs settled {settled}) -- if this is equal, the relative \
             `top` offset is not being applied"
        );
        assert!(
            entering <= settled + CONTENT_RISE_PX + 0.5,
            "the rise overshot its own {CONTENT_RISE_PX}px budget: {entering}"
        );
    }

    /// Pins that no timer outlives the window: closing it drops the view
    /// entity, so nothing can be notified and no further frame requested.
    #[gpui::test]
    fn closing_the_window_drops_the_view_and_with_it_the_frame_loop(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| AboutView::new(window, cx, false, None));
        let weak = window
            .update(cx, |_view, _window, cx| cx.entity().downgrade())
            .expect("the window is open");
        let visual = VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        assert!(weak.upgrade().is_some(), "sanity: the view is alive");

        window
            .update(cx, |_view, window, _cx| window.remove_window())
            .expect("the window is still open");
        cx.run_until_parked();

        assert!(
            weak.upgrade().is_none(),
            "the About view outlived its window -- something is still holding a \
             strong reference, and an animation-frame callback could still fire"
        );
    }

    /// `wants_animation_frame` is the only thing that keeps the loop alive,
    /// so its rest state has to be genuinely quiet.
    #[gpui::test]
    fn a_settled_window_asks_for_no_more_frames(cx: &mut TestAppContext) {
        let window = cx.add_window(|window, cx| AboutView::new(window, cx, false, None));
        window
            .update(cx, |view, _window, _cx| {
                let now = Instant::now();
                view.opened_at = now
                    .checked_sub(ENTRANCE_TOTAL * 2)
                    .expect("test clock is well past the epoch");
                assert!(
                    !view.wants_animation_frame(now),
                    "a settled About window must stop requesting frames"
                );
                view.reduce_motion = true;
                assert!(!view.wants_animation_frame(now));
            })
            .expect("the window is open");
    }
}
