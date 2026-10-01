//! macOS media-key / Control Center integration (docs/UX-SPEC.md §2's implicit
//! media-key support: `MPNowPlayingInfoCenter` publish,
//! `MPRemoteCommandCenter` play/pause/toggle/seek).
//!
//! `playbackState` must be set explicitly -- macOS doesn't infer it from
//! an audio session the way iOS does. `addTargetWithHandler` needs a
//! `block2::RcBlock` (not a bare Rust closure), and a real `NSApplication`
//! run loop is required for the handler blocks to fire at all -- GPUI
//! already provides that on macOS, so nothing extra is needed here beyond
//! registering after the window opens.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommandCenter,
    MPRemoteCommandEvent, MPRemoteCommandHandlerStatus, MPSkipIntervalCommandEvent,
};

#[derive(Debug, Clone)]
pub(crate) struct NowPlayingState {
    pub title: String,
    pub elapsed_secs: f64,
    pub duration_secs: f64,
    pub paused: bool,
}

/// One command from Control Center's Now Playing widget or a physical
/// media key. Forwarded down an `UnboundedSender` from whatever thread
/// AppKit invokes the handler block on, bridged into `Root` by
/// `main.rs::spawn_now_playing_task` -- the same "raw event channel ->
/// `cx.spawn` -> `Root::on_*`" shape `spawn_player_events_task` already
/// uses for `player.events()`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RemoteCommand {
    Play,
    Pause,
    Toggle,
    SeekRelative(f64),
    SeekAbsolute(f64),
}

/// Holds the opaque target tokens `addTargetWithHandler` returns -- must
/// stay alive for the app's whole life, or the handlers are torn down.
pub(crate) struct NowPlaying {
    _targets: Vec<Retained<AnyObject>>,
}

impl NowPlaying {
    /// Registers all six remote-command handlers. Must run on the main
    /// thread, once, any time after the window has opened (no
    /// bundle/entitlement is required, just `NSApplication` running with
    /// a non-prohibited activation policy, which GPUI already sets up).
    /// Returns `None` if not on the main thread -- never fatal to playback,
    /// just means no Now Playing integration for this run.
    ///
    /// `skip_back_secs`/
    /// `skip_forward_secs` seed the Control Center widget's own skip-
    /// interval buttons with the app's configured skip lengths (was a
    /// hardcoded 15s for both directions) -- see `set_skip_intervals` for
    /// re-applying a later change without a full re-`register`.
    pub(crate) fn register(
        tx: tokio::sync::mpsc::UnboundedSender<RemoteCommand>,
        skip_back_secs: f64,
        skip_forward_secs: f64,
    ) -> Option<Self> {
        MainThreadMarker::new()?;
        let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
        let mut targets = Vec::new();
        let fwd_intervals =
            NSArray::from_retained_slice(&[NSNumber::numberWithDouble(skip_forward_secs)]);
        let bwd_intervals =
            NSArray::from_retained_slice(&[NSNumber::numberWithDouble(skip_back_secs)]);

        macro_rules! register_simple {
            ($cmd:expr, $variant:expr, $context:expr) => {{
                let cmd = $cmd;
                unsafe { cmd.setEnabled(true) };
                let s = tx.clone();
                let handler = block2::RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                    let status = crate::panic_log::catch_and_log(
                        $context,
                        std::panic::AssertUnwindSafe(|| {
                            let _ = s.send($variant);
                            MPRemoteCommandHandlerStatus::Success
                        }),
                    );
                    status.unwrap_or(MPRemoteCommandHandlerStatus::CommandFailed)
                });
                targets.push(unsafe { cmd.addTargetWithHandler(&handler) });
                std::mem::forget(handler);
            }};
        }

        register_simple!(
            unsafe { center.playCommand() },
            RemoteCommand::Play,
            "MPRemoteCommandCenter play handler"
        );
        register_simple!(
            unsafe { center.pauseCommand() },
            RemoteCommand::Pause,
            "MPRemoteCommandCenter pause handler"
        );
        register_simple!(
            unsafe { center.togglePlayPauseCommand() },
            RemoteCommand::Toggle,
            "MPRemoteCommandCenter togglePlayPause handler"
        );

        unsafe {
            let skip_fwd = center.skipForwardCommand();
            skip_fwd.setEnabled(true);
            skip_fwd.setPreferredIntervals(&fwd_intervals);
            let s = tx.clone();
            let handler = block2::RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
                let status = crate::panic_log::catch_and_log(
                    "MPRemoteCommandCenter skipForward handler",
                    std::panic::AssertUnwindSafe(|| {
                        let ev = event.as_ref();
                        let Some(skip) = ev.downcast_ref::<MPSkipIntervalCommandEvent>() else {
                            tracing::warn!(
                                "skipForward delivered a non-MPSkipIntervalCommandEvent"
                            );
                            return MPRemoteCommandHandlerStatus::CommandFailed;
                        };
                        let _ = s.send(RemoteCommand::SeekRelative(skip.interval()));
                        MPRemoteCommandHandlerStatus::Success
                    }),
                );
                status.unwrap_or(MPRemoteCommandHandlerStatus::CommandFailed)
            });
            targets.push(skip_fwd.addTargetWithHandler(&handler));
            std::mem::forget(handler);

            let skip_bwd = center.skipBackwardCommand();
            skip_bwd.setEnabled(true);
            skip_bwd.setPreferredIntervals(&bwd_intervals);
            let s = tx.clone();
            let handler = block2::RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
                let status = crate::panic_log::catch_and_log(
                    "MPRemoteCommandCenter skipBackward handler",
                    std::panic::AssertUnwindSafe(|| {
                        let ev = event.as_ref();
                        let Some(skip) = ev.downcast_ref::<MPSkipIntervalCommandEvent>() else {
                            tracing::warn!(
                                "skipBackward delivered a non-MPSkipIntervalCommandEvent"
                            );
                            return MPRemoteCommandHandlerStatus::CommandFailed;
                        };
                        let _ = s.send(RemoteCommand::SeekRelative(-skip.interval()));
                        MPRemoteCommandHandlerStatus::Success
                    }),
                );
                status.unwrap_or(MPRemoteCommandHandlerStatus::CommandFailed)
            });
            targets.push(skip_bwd.addTargetWithHandler(&handler));
            std::mem::forget(handler);

            let scrub = center.changePlaybackPositionCommand();
            scrub.setEnabled(true);
            let handler = block2::RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
                let status = crate::panic_log::catch_and_log(
                    "MPRemoteCommandCenter changePlaybackPosition handler",
                    std::panic::AssertUnwindSafe(|| {
                        let ev = event.as_ref();
                        let Some(pos) = ev.downcast_ref::<MPChangePlaybackPositionCommandEvent>()
                        else {
                            tracing::warn!(
                                "changePlaybackPosition delivered a \
                                 non-MPChangePlaybackPositionCommandEvent"
                            );
                            return MPRemoteCommandHandlerStatus::CommandFailed;
                        };
                        let _ = tx.send(RemoteCommand::SeekAbsolute(pos.positionTime()));
                        MPRemoteCommandHandlerStatus::Success
                    }),
                );
                status.unwrap_or(MPRemoteCommandHandlerStatus::CommandFailed)
            });
            targets.push(scrub.addTargetWithHandler(&handler));
            std::mem::forget(handler);
        }

        Some(NowPlaying { _targets: targets })
    }

    /// Re-applies the Control
    /// Center widget's own skip-interval buttons after a mid-session
    /// change to the configured skip lengths (`Root::set_skip_length`) --
    /// `MPRemoteCommandCenter::sharedCommandCenter()` is a singleton, so
    /// re-fetching it here and calling `setPreferredIntervals` again on the
    /// already-registered commands is sufficient; there is no need to
    /// re-run `register`'s full handler setup (which would also leak a
    /// second copy of every `RcBlock`/target token).
    pub(crate) fn set_skip_intervals(&self, skip_back_secs: f64, skip_forward_secs: f64) {
        unsafe {
            let center = MPRemoteCommandCenter::sharedCommandCenter();
            let fwd_intervals =
                NSArray::from_retained_slice(&[NSNumber::numberWithDouble(skip_forward_secs)]);
            let bwd_intervals =
                NSArray::from_retained_slice(&[NSNumber::numberWithDouble(skip_back_secs)]);
            center
                .skipForwardCommand()
                .setPreferredIntervals(&fwd_intervals);
            center
                .skipBackwardCommand()
                .setPreferredIntervals(&bwd_intervals);
        }
    }

    /// Publishes current title/elapsed/duration/rate and explicit
    /// playback state -- macOS never infers this.
    pub(crate) fn publish(&self, state: NowPlayingState) {
        unsafe {
            let center = MPNowPlayingInfoCenter::defaultCenter();
            let title = NSString::from_str(&state.title);
            let duration = NSNumber::numberWithDouble(state.duration_secs.max(0.0));
            let elapsed = NSNumber::numberWithDouble(state.elapsed_secs.max(0.0));
            let rate = NSNumber::numberWithDouble(if state.paused { 0.0 } else { 1.0 });
            let keys: [&NSString; 4] = [
                MPMediaItemPropertyTitle,
                MPMediaItemPropertyPlaybackDuration,
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                MPNowPlayingInfoPropertyPlaybackRate,
            ];
            let objects: [&AnyObject; 4] = [&*title, &*duration, &*elapsed, &*rate];
            let dict = NSDictionary::from_slices(&keys, &objects);
            center.setNowPlayingInfo(Some(&dict));
            center.setPlaybackState(if state.paused {
                MPNowPlayingPlaybackState::Paused
            } else {
                MPNowPlayingPlaybackState::Playing
            });
        }
    }

    /// Clears the Now Playing widget (called on Esc-stop).
    pub(crate) fn clear(&self) {
        unsafe {
            let center = MPNowPlayingInfoCenter::defaultCenter();
            center.setNowPlayingInfo(None);
            center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        }
    }
}
