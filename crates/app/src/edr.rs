//! HDR output decisions for the video layer (crates/app/ARCHITECTURE.md
//! "HDR output"). Pure functions only; `gl_video.rs` gathers the inputs from
//! AppKit and mpv and applies the results.

use jellyfin_api::models::VideoRangeType;
use player::{OutputTarget, TargetPrimaries, REFERENCE_WHITE_NITS};

/// How often the geometry driver re-reads screen headroom and the playing
/// transfer: headroom ramps over about two seconds after EDR engages and
/// follows the brightness slider, neither of which needs per-frame tracking.
pub(crate) const EDR_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// One screen's EDR state as AppKit reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScreenHeadroom {
    /// `maximumExtendedDynamicRangeColorComponentValue`: what the compositor
    /// clamps to right now.
    pub current: f64,
    /// `maximumPotentialExtendedDynamicRangeColorComponentValue`: the most
    /// the display can ever offer; 1.0 means no EDR.
    pub potential: f64,
    /// The screen's colour space is wider than sRGB (P3 panels).
    pub wide_gamut: bool,
}

/// The transfer the server reports for the video about to load. Applying
/// its target before `Player::load` means the first frame already renders in
/// the right range, so SDR titles and HDR titles never switch mid-play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RangeHint {
    /// No usable metadata (or Dolby Vision profile 5): mpv's decoded transfer decides.
    Unknown = 0,
    Sdr = 1,
    Pq = 2,
    Hlg = 3,
}

impl RangeHint {
    /// Dolby Vision with an HDR10, HLG or SDR base layer follows its base,
    /// which is what mpv decodes.
    pub(crate) const fn from_range_type(range: Option<&VideoRangeType>) -> Self {
        match range {
            Some(
                VideoRangeType::Hdr10
                | VideoRangeType::Hdr10Plus
                | VideoRangeType::DoviWithHdr10
                | VideoRangeType::DoviWithHdr10Plus
                | VideoRangeType::DoviWithEl
                | VideoRangeType::DoviWithElhdr10Plus,
            ) => Self::Pq,
            Some(VideoRangeType::Hlg | VideoRangeType::DoviWithHlg) => Self::Hlg,
            Some(VideoRangeType::Sdr | VideoRangeType::DoviWithSdr) => Self::Sdr,
            _ => Self::Unknown,
        }
    }

    pub(crate) const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Sdr,
            2 => Self::Pq,
            3 => Self::Hlg,
            _ => Self::Unknown,
        }
    }

    const fn transfer(self) -> Option<&'static str> {
        match self {
            Self::Unknown => None,
            Self::Sdr => Some("sdr"),
            Self::Pq => Some("pq"),
            Self::Hlg => Some("hlg"),
        }
    }
}

/// mpv's decoded transfer once a frame exists, the server's hint before:
/// what mpv actually decodes always wins over metadata.
pub(crate) fn effective_transfer(decoded: Option<&str>, hint: RangeHint) -> Option<&str> {
    decoded.or(hint.transfer())
}

/// The surface is 16-bit float only when some attached display can show
/// EDR and AppKit can request it: a half-float drawable costs twice the
/// memory bandwidth of RGBA8 on every present, which buys nothing on an
/// SDR-only Mac, and the pixel format is fixed for the context's life.
pub(crate) fn wants_float_surface(max_potential_headroom: f64, view_edr_supported: bool) -> bool {
    view_edr_supported && max_potential_headroom > 1.0
}

/// PQ (HDR10, Dolby Vision base layers) and HLG are the transfers with
/// light above SDR white; everything else stays on the SDR path.
pub(crate) fn is_hdr_transfer(transfer: &str) -> bool {
    matches!(transfer, "pq" | "hlg")
}

/// Headroom mpv tone-maps to: the current value clamped to `[1, potential]`
/// and floored to 1/8 stop, so brightness jitter does not rebuild mpv's
/// shaders every poll and the target never exceeds what the compositor shows.
pub(crate) fn quantized_headroom(current: f64, potential: f64) -> f64 {
    let ceiling = potential.max(1.0);
    let h = if current.is_finite() {
        current.clamp(1.0, ceiling)
    } else {
        1.0
    };
    let stops = (h.log2() * 8.0).floor() / 8.0;
    stops.exp2()
}

/// mpv's colour target for the current content and screen: extended-range
/// linear only for HDR content on a float surface whose screen has EDR
/// potential; anything else keeps mpv's default SDR output.
pub(crate) fn output_target(
    float_surface: bool,
    transfer: Option<&str>,
    screen: Option<ScreenHeadroom>,
) -> OutputTarget {
    let Some(screen) = screen else {
        return OutputTarget::Sdr;
    };
    let hdr = transfer.is_some_and(is_hdr_transfer);
    if !float_surface || !hdr || screen.potential <= 1.0 {
        return OutputTarget::Sdr;
    }
    OutputTarget::ExtendedLinear {
        // Output is display-referred (no colour matching on the GL surface),
        // so the primaries are the panel's own.
        primaries: if screen.wide_gamut {
            TargetPrimaries::DisplayP3
        } else {
            TargetPrimaries::Bt709
        },
        peak_nits: REFERENCE_WHITE_NITS * quantized_headroom(screen.current, screen.potential),
    }
}

/// The EDR request on the video view is made once, the first time HDR
/// content needs it: AppKit only reads the request when a view first gets
/// its GL surface, and holding it raises display power draw.
pub(crate) fn should_request_edr(already_requested: bool, target: &OutputTarget) -> bool {
    !already_requested && matches!(target, OutputTarget::ExtendedLinear { .. })
}

/// The info overlay's "HDR output" row: what the viewer is actually getting,
/// stated plainly, because HDR in real footage is hard to judge by eye.
pub(crate) fn output_readout(
    transfer: Option<&str>,
    applied_trc: Option<&str>,
    applied_peak_nits: Option<f64>,
    float_surface: bool,
    screen_potential: f64,
) -> String {
    if !transfer.is_some_and(is_hdr_transfer) {
        return "SDR content".to_string();
    }
    if applied_trc == Some("linear") {
        let peak = applied_peak_nits.unwrap_or(REFERENCE_WHITE_NITS);
        return format!(
            "EDR, peak {peak:.0} nits ({:.1}x SDR white)",
            peak / REFERENCE_WHITE_NITS
        );
    }
    if !float_surface || screen_potential <= 1.0 {
        "Tone-mapped to SDR (this display has no EDR)".to_string()
    } else {
        "Tone-mapped to SDR (EDR not engaged)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XDR: ScreenHeadroom = ScreenHeadroom {
        current: 4.0,
        potential: 16.0,
        wide_gamut: true,
    };
    const SDR_SCREEN: ScreenHeadroom = ScreenHeadroom {
        current: 1.0,
        potential: 1.0,
        wide_gamut: false,
    };

    #[test]
    fn float_surface_only_with_edr_capable_display_and_api() {
        assert!(wants_float_surface(16.0, true));
        assert!(!wants_float_surface(1.0, true));
        assert!(!wants_float_surface(16.0, false));
        assert!(!wants_float_surface(0.0, true));
    }

    #[test]
    fn hdr_transfers_are_pq_and_hlg() {
        assert!(is_hdr_transfer("pq"));
        assert!(is_hdr_transfer("hlg"));
        for sdr in ["bt.1886", "srgb", "gamma2.2", "linear", "auto", ""] {
            assert!(!is_hdr_transfer(sdr), "{sdr}");
        }
    }

    #[test]
    fn headroom_is_clamped_and_floored_to_eighth_stops() {
        assert_eq!(quantized_headroom(4.0, 16.0), 4.0);
        assert_eq!(quantized_headroom(0.5, 16.0), 1.0);
        assert_eq!(quantized_headroom(30.0, 16.0), 16.0);
        assert_eq!(quantized_headroom(3.0, 1.0), 1.0);
        assert_eq!(quantized_headroom(f64::NAN, 16.0), 1.0);
        let h = quantized_headroom(3.49, 16.0);
        assert!(h <= 3.49 && h > 3.49 / 2f64.powf(1.0 / 8.0), "{h}");
        assert_eq!(
            quantized_headroom(3.49, 16.0),
            quantized_headroom(3.45, 16.0)
        );
    }

    #[test]
    fn hdr_on_edr_screen_targets_extended_linear_at_headroom_times_reference_white() {
        assert_eq!(
            output_target(true, Some("pq"), Some(XDR)),
            OutputTarget::ExtendedLinear {
                primaries: TargetPrimaries::DisplayP3,
                peak_nits: REFERENCE_WHITE_NITS * 4.0,
            }
        );
        let narrow = ScreenHeadroom {
            wide_gamut: false,
            ..XDR
        };
        assert!(matches!(
            output_target(true, Some("hlg"), Some(narrow)),
            OutputTarget::ExtendedLinear {
                primaries: TargetPrimaries::Bt709,
                ..
            }
        ));
    }

    #[test]
    fn before_headroom_ramps_the_target_is_reference_white() {
        let ramping = ScreenHeadroom {
            current: 1.0,
            ..XDR
        };
        assert_eq!(
            output_target(true, Some("pq"), Some(ramping)),
            OutputTarget::ExtendedLinear {
                primaries: TargetPrimaries::DisplayP3,
                peak_nits: REFERENCE_WHITE_NITS,
            }
        );
    }

    #[test]
    fn everything_else_keeps_the_sdr_path() {
        assert_eq!(
            output_target(true, Some("bt.1886"), Some(XDR)),
            OutputTarget::Sdr
        );
        assert_eq!(output_target(true, None, Some(XDR)), OutputTarget::Sdr);
        assert_eq!(
            output_target(false, Some("pq"), Some(XDR)),
            OutputTarget::Sdr
        );
        assert_eq!(
            output_target(true, Some("pq"), Some(SDR_SCREEN)),
            OutputTarget::Sdr
        );
        assert_eq!(output_target(true, Some("pq"), None), OutputTarget::Sdr);
    }

    #[test]
    fn edr_is_requested_once_and_only_for_extended_targets() {
        let edr = output_target(true, Some("pq"), Some(XDR));
        assert!(should_request_edr(false, &edr));
        assert!(!should_request_edr(true, &edr));
        assert!(!should_request_edr(false, &OutputTarget::Sdr));
    }

    #[test]
    fn readout_states_what_the_viewer_gets() {
        assert_eq!(
            output_readout(Some("bt.1886"), None, None, true, 16.0),
            "SDR content"
        );
        assert_eq!(
            output_readout(Some("pq"), Some("linear"), Some(2030.0), true, 16.0),
            "EDR, peak 2030 nits (10.0x SDR white)"
        );
        assert_eq!(
            output_readout(Some("hlg"), Some("auto"), None, false, 1.0),
            "Tone-mapped to SDR (this display has no EDR)"
        );
        assert_eq!(
            output_readout(Some("pq"), Some("auto"), None, true, 16.0),
            "Tone-mapped to SDR (EDR not engaged)"
        );
    }

    #[test]
    fn range_hint_follows_the_base_layer() {
        let hint = |r| RangeHint::from_range_type(Some(&r));
        assert_eq!(hint(VideoRangeType::Hdr10), RangeHint::Pq);
        assert_eq!(hint(VideoRangeType::DoviWithHdr10), RangeHint::Pq);
        assert_eq!(hint(VideoRangeType::DoviWithHlg), RangeHint::Hlg);
        assert_eq!(hint(VideoRangeType::DoviWithSdr), RangeHint::Sdr);
        assert_eq!(hint(VideoRangeType::Dovi), RangeHint::Unknown);
        assert_eq!(hint(VideoRangeType::Unknown), RangeHint::Unknown);
        assert_eq!(RangeHint::from_range_type(None), RangeHint::Unknown);
        for h in [
            RangeHint::Unknown,
            RangeHint::Sdr,
            RangeHint::Pq,
            RangeHint::Hlg,
        ] {
            assert_eq!(RangeHint::from_u8(h as u8), h);
        }
    }

    #[test]
    fn decoded_transfer_wins_and_the_hint_covers_the_first_frame() {
        assert_eq!(effective_transfer(None, RangeHint::Pq), Some("pq"));
        assert_eq!(
            effective_transfer(Some("bt.1886"), RangeHint::Pq),
            Some("bt.1886")
        );
        assert_eq!(effective_transfer(None, RangeHint::Unknown), None);
        let first_frame = output_target(true, effective_transfer(None, RangeHint::Pq), Some(XDR));
        assert!(matches!(first_frame, OutputTarget::ExtendedLinear { .. }));
        let sdr = output_target(true, effective_transfer(None, RangeHint::Sdr), Some(XDR));
        assert_eq!(sdr, OutputTarget::Sdr);
    }
}
