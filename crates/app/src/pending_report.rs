//! Quit-time playback-report persistence.
//!
//! GPUI's `on_app_quit` gives shutdown futures `gpui::SHUTDOWN_TIMEOUT`
//! (100ms) before the process exits regardless. Over the primary deployment
//! link (a ~118ms-RTT VPN) no HTTP round trip can complete inside that
//! window, so any final `Stopped` report attempted at quit is lost along
//! with the process — the server keeps a dangling `Sessions/Playing` record
//! and the last-reported position goes stale. Rather than pretend the
//! in-window attempt is a guarantee, `Root::prepare_for_quit` persists the
//! pending report here (a cheap local fsync easily fits the window) and the
//! next launch replays it once a session to the same server is established.
//!
//! Replay is best-effort with bounded retries. The file is deleted on
//! successful delivery, on expiry ([`MAX_AGE_SECS`]), or when unreadable; a
//! failed replay leaves it for the launch after. Replaying a report the
//! quit-time attempt actually managed to deliver is harmless — same
//! payload, same `play_session_id`, so the server's final state is
//! unchanged.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const FILE_NAME: &str = "pending-stop-report.json";

/// Replay window. Long enough to cover the common "quit at night, relaunch
/// next evening" gap; short enough that a report from a forgotten machine
/// can't clobber a resume position the user has since moved on another
/// device. Past this the local mirror's own position write (committed at
/// quit) is still the in-app source of truth.
const MAX_AGE_SECS: u64 = 48 * 60 * 60;

/// Bounded replay attempts per launch: the session was just established, so
/// the server is almost certainly reachable; a couple of retries cover a
/// transient hiccup without holding a background task open for long.
const REPLAY_ATTEMPTS: u32 = 3;
const REPLAY_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingStopReport {
    pub base_url: String,
    pub item_id: String,
    pub media_source_id: String,
    pub play_session_id: String,
    pub position_ticks: i64,
    pub saved_at_unix_secs: u64,
}

/// Why [`claim`] did or didn't hand a persisted report back for replay.
/// Pulled out as a pure decision (no I/O, injected clock) so the
/// freshness/matching rules are directly testable.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Replay it against the just-connected server.
    Replay,
    /// Belongs to a different server; leave the file for a later connect.
    WrongServer,
    /// Older than [`MAX_AGE_SECS`]; delete without replaying.
    Stale,
}

pub(crate) fn claim(report: &PendingStopReport, base_url: &str, now_unix_secs: u64) -> Claim {
    // Saturating: a clock that moved backwards must never turn a fresh
    // report stale (or a stale one fresh via overflow).
    if now_unix_secs.saturating_sub(report.saved_at_unix_secs) > MAX_AGE_SECS {
        return Claim::Stale;
    }
    if report.base_url.trim_end_matches('/') != base_url.trim_end_matches('/') {
        return Claim::WrongServer;
    }
    Claim::Replay
}

fn file_path() -> PathBuf {
    crate::paths::state_root().join(FILE_NAME)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persist `report` for next-launch replay. Failures are logged, never
/// propagated — the quit path must not stall on a disk error.
pub(crate) fn persist(report: &PendingStopReport) {
    let bytes = match serde_json::to_vec(report) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize pending stop report");
            return;
        }
    };
    if let Err(e) = crate::paths::write_private(&file_path(), &bytes) {
        tracing::warn!(error = %e, "failed to persist pending stop report");
    } else {
        tracing::info!(
            item_id = %report.item_id,
            position_ticks = report.position_ticks,
            "persisted pending stop report for next-launch replay"
        );
    }
}

fn delete_file() {
    let _ = std::fs::remove_file(file_path());
}

/// Load the persisted report if the just-connected `base_url` should replay
/// it. Deletes the file when it is unreadable or stale; leaves it in place
/// both for a different server's report and for the returned report itself
/// (deletion after a successful replay is the caller's confirmation).
pub(crate) fn load_for_replay(base_url: &str) -> Option<PendingStopReport> {
    let bytes = std::fs::read(file_path()).ok()?;
    let report: PendingStopReport = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "pending stop report unreadable; discarding");
            delete_file();
            return None;
        }
    };
    match claim(&report, base_url, now_unix_secs()) {
        Claim::Replay => Some(report),
        Claim::WrongServer => None,
        Claim::Stale => {
            tracing::info!(
                item_id = %report.item_id,
                "pending stop report expired unreplayed; discarding"
            );
            delete_file();
            None
        }
    }
}

/// Deliver a claimed report with bounded retries; deletes the file only on
/// success. Runs on the tokio runtime (spawned by `Root` right after a
/// session is established).
pub(crate) async fn replay(client: jellyfin_api::JellyfinClient, report: PendingStopReport) {
    let payload = jellyfin_api::PlaybackReport {
        kind: jellyfin_api::PlaybackReportKind::Stopped,
        item_id: report.item_id.clone(),
        media_source_id: report.media_source_id.clone(),
        position_ticks: report.position_ticks,
        is_paused: false,
        // Matches ReportingSession's DEFAULT_VOLUME: no volume control is
        // wired into reporting yet, and a final report with a misleadingly
        // low volume is worse than a constant.
        volume_level: 100,
        audio_stream_index: None,
        subtitle_stream_index: None,
        play_session_id: report.play_session_id.clone(),
    };
    for attempt in 1..=REPLAY_ATTEMPTS {
        match client.report_playback(payload.clone()).await {
            Ok(()) => {
                tracing::info!(
                    item_id = %report.item_id,
                    position_ticks = report.position_ticks,
                    "replayed quit-time stop report from previous launch"
                );
                delete_file();
                return;
            }
            Err(e) if attempt == REPLAY_ATTEMPTS => {
                tracing::warn!(
                    error = %e,
                    item_id = %report.item_id,
                    "pending stop report replay failed; leaving it for the next launch"
                );
            }
            Err(_) => tokio::time::sleep(REPLAY_RETRY_DELAY).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(base_url: &str, saved_at: u64) -> PendingStopReport {
        PendingStopReport {
            base_url: base_url.into(),
            item_id: "item".into(),
            media_source_id: "source".into(),
            play_session_id: "session".into(),
            position_ticks: 42,
            saved_at_unix_secs: saved_at,
        }
    }

    #[test]
    fn claim_replays_a_fresh_report_for_the_same_server() {
        let r = report("http://server:8096", 1_000);
        assert_eq!(claim(&r, "http://server:8096", 1_000 + 60), Claim::Replay);
    }

    #[test]
    fn claim_tolerates_trailing_slash_differences() {
        // The session store and the client normalize slashes independently;
        // a cosmetic mismatch must not strand the report forever.
        let r = report("http://server:8096/", 1_000);
        assert_eq!(claim(&r, "http://server:8096", 1_000), Claim::Replay);
    }

    #[test]
    fn claim_leaves_another_servers_report_alone() {
        let r = report("http://other:8096", 1_000);
        assert_eq!(claim(&r, "http://server:8096", 1_000), Claim::WrongServer);
    }

    #[test]
    fn claim_expires_a_stale_report_even_for_the_matching_server() {
        let r = report("http://server:8096", 1_000);
        assert_eq!(
            claim(&r, "http://server:8096", 1_000 + MAX_AGE_SECS + 1),
            Claim::Stale
        );
    }

    #[test]
    fn claim_survives_a_clock_that_moved_backwards() {
        let r = report("http://server:8096", 1_000);
        assert_eq!(claim(&r, "http://server:8096", 0), Claim::Replay);
    }

    #[test]
    fn report_round_trips_through_json() {
        let r = report("http://server:8096", 123);
        let bytes = serde_json::to_vec(&r).expect("serialize");
        let back: PendingStopReport = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(back, r);
    }
}
