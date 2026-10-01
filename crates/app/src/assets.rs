//! `AssetSource` wiring so `gpui::svg().path("icons/....svg")` resolves
//! to something -- GPUI's default `AssetSource` is `()`, which returns
//! `Ok(None)` for every path (`gpui-0.2.2/src/assets.rs`), so without this
//! every OSD icon would silently paint nothing. `rust-embed` bakes
//! `assets/**` into the binary at compile time (already a transitive dep of
//! `gpui` itself per `Cargo.lock`, so this adds zero new supply-chain
//! surface) -- zero-network, zero-runtime-file-IO, matching the rest of the
//! app's "cache/bundle everything, never block on disk/network for UI"
//! posture (docs/DATA.md).
//!
//! Wired into `Application::new().with_assets(Assets)` in `main.rs`.
//!
//! Everything under `crates/app/assets/` is embedded from one root: the
//! vendored Lucide icons (`icons/`), the OSD scrim (`scrim/`), the brand
//! fonts (`fonts/`, Part A §3 of `docs/DESIGN-GUIDE.md`) and the mascot
//! art (`brand/`, §A.4). `scripts/make-icon.sh` builds `AppIcon.icns` from
//! the same `brand/jellybeam/icon-512.png`, so the brand has one source of
//! truth.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// `crates/app/assets/`: icons, scrim, fonts and brand art.
#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
struct Embedded;

fn embedded(path: &str) -> Option<Cow<'static, [u8]>> {
    Embedded::get(path).map(|f| f.data)
}

pub(crate) struct Assets;

/// Brand §3's three faces, as the exact embedded paths `main.rs` hands to
/// `TextSystem::add_fonts` at startup. Listed here (rather than inline in
/// `main.rs`) so `every_brand_font_is_embedded` below can assert against the
/// same list the app actually loads -- a typo'd filename would otherwise
/// leave the app silently falling back to the system UI font with no error,
/// the font-shaped analog of the icon bug `every_referenced_icon_is_embedded`
/// guards against.
///
/// Archivo 400/500/600/700/800 (`theme::FONT_UI`), Martian Mono 400/700
/// (`theme::FONT_MONO`), Bagel Fat One (`theme::FONT_DISPLAY`, the word
/// "Jellybeam" and nothing else).
pub(crate) const BRAND_FONTS: [&str; 8] = [
    "fonts/Archivo-400.ttf",
    "fonts/Archivo-500.ttf",
    "fonts/Archivo-600.ttf",
    "fonts/Archivo-700.ttf",
    "fonts/Archivo-800.ttf",
    "fonts/MartianMono-400.ttf",
    "fonts/MartianMono-700.ttf",
    "fonts/BagelFatOne-Regular.ttf",
];

/// The raw bytes of every [`BRAND_FONTS`] entry, in `TextSystem::add_fonts`'
/// own `Vec<Cow<'static, [u8]>>` shape. `rust_embed` hands back a `Cow`
/// borrowed straight from the binary's own rodata in release builds, so this
/// copies nothing.
///
/// A missing entry is skipped rather than panicking: a font that failed to
/// embed degrades to the system UI face, which is ugly but usable, and
/// `every_brand_font_is_embedded` already fails the build if it ever
/// happens. Losing the window over a typo would be the worse trade.
pub(crate) fn brand_font_data() -> Vec<Cow<'static, [u8]>> {
    BRAND_FONTS
        .iter()
        .filter_map(|path| embedded(path))
        .collect()
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(embedded(path))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Embedded::iter()
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against a typo'd icon filename ever landing silently -- every
    /// path `player_ui.rs` references via `osd_icon_button`/`svg().path()`
    /// must actually be embedded, or the icon renders as nothing at
    /// runtime with no error.
    #[test]
    fn every_referenced_icon_is_embedded() {
        let expected = [
            "icons/play.svg",
            "icons/pause.svg",
            "icons/square.svg",
            "icons/rotate-ccw.svg",
            "icons/rotate-cw.svg",
            "icons/skip-back.svg",
            "icons/skip-forward.svg",
            "icons/volume-2.svg",
            "icons/volume-x.svg",
            "icons/captions.svg",
            "icons/audio-lines.svg",
            "icons/info.svg",
            "icons/picture-in-picture-2.svg",
            "icons/maximize.svg",
            "icons/minimize.svg",
            "icons/x.svg",
        ];
        for path in expected {
            assert!(
                embedded(path).is_some(),
                "expected embedded asset at {path}"
            );
        }
    }

    /// Same guard, for brand §3's type. Without this a renamed/missing TTF
    /// shows up only as "the app is in the system font again" -- no error,
    /// no log line, and easy to miss in a screenshot.
    #[test]
    fn every_brand_font_is_embedded() {
        for path in BRAND_FONTS {
            assert!(embedded(path).is_some(), "expected embedded font at {path}");
        }
        assert_eq!(brand_font_data().len(), BRAND_FONTS.len());
    }

    /// §5's `jb_mascot_*` poses that have a screen, rendered wherever
    /// `root.rs::mascot_image` places one via `img()` (full-colour PNG
    /// rasterization through the same `AssetSource`), so a missing file
    /// would paint an empty box instead of the mascot. Poses without a
    /// screen are not embedded (every file under `assets/` ships in the
    /// binary).
    #[test]
    fn every_shipped_mascot_pose_is_embedded() {
        let expected = [
            "brand/jellybeam/jb_mascot_base.png",
            "brand/jellybeam/jb_mascot_watching.png",
            "brand/jellybeam/jb_mascot_curious.png",
            "brand/jellybeam/jb_mascot_searching.png",
        ];
        for path in expected {
            assert!(
                embedded(path).is_some(),
                "expected embedded asset at {path}"
            );
        }
    }
}

#[cfg(test)]
mod img_source_tests {
    /// `img()`'s `From<&str>` picks between an HTTP fetch and an
    /// `AssetSource` lookup by running the string through
    /// `http_client::Uri::from_str` -- so a bare embedded path that happened
    /// to parse as a relative URI reference would be silently routed to the
    /// network and render nothing. Pinning the classification here rather
    /// than discovering it in a screenshot.
    #[test]
    fn the_mascot_path_resolves_as_an_embedded_asset_not_a_uri() {
        let source = gpui::ImageSource::from("brand/jellybeam/jb_mascot_base.png");
        assert!(
            matches!(
                source,
                gpui::ImageSource::Resource(gpui::Resource::Embedded(_))
            ),
            "brand/jellybeam/jb_mascot_base.png must resolve through the AssetSource, \
             not as a URI"
        );
    }
}
