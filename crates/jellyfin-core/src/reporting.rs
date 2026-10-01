//! Playback reporting state machine: start/progress(10s)/stopped scheduling,
//! pause/seek/track-change edge reports, retry-with-backoff.
//!
//! Timing/retry logic is generic over a private [`ReportSink`] trait (real
//! `JellyfinClient` path, fake in tests driven by `tokio::time::pause` +
//! `advance`) so unit tests don't need a live server.
//!
//! The final `Stopped` report is processed by the session's actor task,
//! ordered after the initial Start, which attempts it inline and hands
//! failures to a shared bounded [`FlushQueue`] rather than spawning
//! per-stop unbounded retry tasks. If the actor doesn't acknowledge within
//! [`STOP_ACK_TIMEOUT`], `stop()` aborts it and enqueues the report itself.
//! None of this survives process exit: the app's quit path persists a
//! pending stop report to disk and replays it on the next launch.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use jellyfin_api::{ApiError, JellyfinClient, PlaybackReport, PlaybackReportKind};
use tokio::sync::{mpsc, oneshot};

use crate::backoff::Backoff;
use crate::ReportContext;

const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(60);
/// Start/progress reports are superseded by the next one 10s later, so we
/// don't retry them forever — only the final Stopped report gets that
/// guarantee (see [`FlushQueue`]).
const TRANSIENT_REPORT_MAX_ATTEMPTS: u32 = 5;
/// No volume control is wired into the frozen `ReportingSession` API yet;
/// report max volume so the server doesn't record a misleadingly low value.
const DEFAULT_VOLUME: u8 = 100;
/// Ceiling on how long the background flush queue retries one final
/// Stopped report before giving up — long enough to cover a Wi-Fi blip or
/// server restart, short enough that a permanently-unreachable server
/// doesn't keep a background task alive for the process's lifetime.
const FLUSH_MAX_DURATION: Duration = Duration::from_secs(30 * 60);
/// Position updates arrive in bursts while the user drags a scrub bar. A
/// detected seek is only reported once updates go quiet for this long — a
/// burst of jumps inside the window extends it, collapsing the whole burst
/// into one report reflecting the final position.
const SEEK_DEBOUNCE: Duration = Duration::from_millis(500);
/// How long `stop()` waits for the actor to acknowledge delivery before
/// aborting it and enqueueing the report itself — equal to
/// `gpui::SHUTDOWN_TIMEOUT`, the entire grace window on app quit.
const STOP_ACK_TIMEOUT: Duration = Duration::from_millis(100);

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait ReportSink: Send + Sync + 'static {
    fn report(&self, report: PlaybackReport) -> BoxFuture<'_, Result<(), ApiError>>;
}

impl ReportSink for JellyfinClient {
    fn report(&self, report: PlaybackReport) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(self.report_playback(report))
    }
}

#[derive(Debug, Clone, Copy)]
struct PlaybackState {
    position_ticks: i64,
    is_paused: bool,
    audio_stream_index: Option<i32>,
    subtitle_stream_index: Option<i32>,
}

enum Cmd {
    Position(i64),
    Pause(bool),
    TrackChange(Option<i32>, Option<i32>),
    /// Stop is processed by the reporting actor rather than directly by
    /// `ReportingSession::stop`. That serializes the final report after the
    /// initial Start attempt, so a slow Start can never land after Stopped.
    Stop {
        ticks: i64,
        completion: oneshot::Sender<()>,
    },
}

/// `#[must_use]`: a `ReportingSession` created and discarded without
/// binding never gets `.stop()` called, so the server never sees the final
/// Stopped report — caught at compile time here; the runtime case (bound
/// but dropped without `.stop()`, e.g. an early return) is caught by this
/// type's [`Drop`] impl instead.
#[must_use = "a ReportingSession must be `.stop()`-ed to send the final Stopped \
              report; dropping it without stopping skips that report (a warning \
              is logged when this happens)"]
pub struct ReportingSession {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    /// Lets `stop()` take ownership of final delivery if the actor is stuck
    /// in a request or retry sleep past the UI's quit grace window.
    task: tokio::task::AbortHandle,
    sink: Arc<dyn ReportSink>,
    ctx: ReportContext,
    /// Set once `.stop()` has run, so `Drop` knows not to warn.
    stopped: bool,
    /// Whether the initial `Start` report was ever delivered. Shared with
    /// `run()`'s task so `stop()` can read it without a round trip through
    /// `cmd_tx`. If it's never acked, a bare Stopped closes no
    /// `Sessions/Playing` record server-side — see [`flush_with_cap`].
    start_acked: Arc<AtomicBool>,
}

impl ReportingSession {
    pub fn start(client: JellyfinClient, ctx: ReportContext) -> Self {
        Self::start_with_sink(Arc::new(client), ctx)
    }

    pub(crate) fn start_with_sink(sink: Arc<dyn ReportSink>, ctx: ReportContext) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let start_acked = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(sink.clone(), ctx.clone(), cmd_rx, start_acked.clone()));
        Self {
            cmd_tx,
            task: task.abort_handle(),
            sink,
            ctx,
            stopped: false,
            start_acked,
        }
    }

    pub fn on_position(&mut self, ticks: i64) {
        let _ = self.cmd_tx.send(Cmd::Position(ticks));
    }

    pub fn on_pause(&mut self, paused: bool) {
        let _ = self.cmd_tx.send(Cmd::Pause(paused));
    }

    pub fn on_track_change(&mut self, audio: Option<i32>, subtitle: Option<i32>) {
        let _ = self.cmd_tx.send(Cmd::TrackChange(audio, subtitle));
    }

    /// Discard a session superseded before playback became active.
    /// Deliberately doesn't emit `Stopped(0)` — a delayed zero-position
    /// report could overwrite a newer session's resume point for the item.
    pub fn abandon(mut self) {
        self.stopped = true;
        self.task.abort();
        tracing::info!(
            item_id = %self.ctx.item_id,
            play_session_id = %self.ctx.play_session_id,
            "reporting session abandoned (superseded before playback became \
             active); any Start already sent will age out server-side"
        );
    }

    /// Read-only view of this session's report identifiers. Lets the app's
    /// quit path persist a pending Stopped report to disk (replayed on next
    /// launch) without this crate knowing about paths or serialization.
    pub fn context(&self) -> &ReportContext {
        &self.ctx
    }

    /// Final position report; consumes the session. Three outcomes, all
    /// ending with the report delivered or in bounded retry:
    /// - the actor processes it in order and acks within [`STOP_ACK_TIMEOUT`]
    /// - the actor is stuck past that window: aborted, report goes to
    ///   [`FlushQueue`]
    /// - the actor is already gone: report goes to [`FlushQueue`] directly
    ///
    /// The queue re-establishes Start first when it was never acked.
    pub async fn stop(mut self, ticks: i64) {
        self.stopped = true;
        let (completion_tx, completion_rx) = oneshot::channel();
        if let Ok(()) = self.cmd_tx.send(Cmd::Stop {
            ticks,
            completion: completion_tx,
        }) {
            if let Ok(Ok(())) = tokio::time::timeout(STOP_ACK_TIMEOUT, completion_rx).await {
                return;
            }
            // Timed out or the actor exited before acking. Abort it so a
            // later Start/Progress retry can't land after the report we're
            // about to queue; FlushQueue's per-session dedup handles a
            // possible race with the actor's own in-flight enqueue or
            // inline POST.
            self.task.abort();
        }
        // Bounded background retry, never inline: an inline attempt here
        // could hold `stop()` for a full request timeout. The queue repairs
        // a never-acked Start before the Stopped (see `flush_with_cap`).
        let report = build_report(
            &self.ctx,
            PlaybackReportKind::Stopped,
            ticks,
            DEFAULT_VOLUME,
            None,
            None,
        );
        flush_queue().enqueue(
            self.sink.clone(),
            self.ctx.clone(),
            self.start_acked.load(Ordering::Relaxed),
            report,
        );
    }
}

/// Actor-side final delivery. When Start was never acked (see
/// [`ReportingSession::start_acked`]), establishes it first — a bare
/// Stopped is ignored server-side otherwise. Falls back to the bounded
/// [`FlushQueue`] on failure.
async fn send_final_stopped(
    sink: &Arc<dyn ReportSink>,
    ctx: &ReportContext,
    start_acked: bool,
    ticks: i64,
) {
    let report = build_report(
        ctx,
        PlaybackReportKind::Stopped,
        ticks,
        DEFAULT_VOLUME,
        None,
        None,
    );
    if !start_acked {
        let start_report = build_report(
            ctx,
            PlaybackReportKind::Start,
            ticks,
            DEFAULT_VOLUME,
            None,
            None,
        );
        if (sink.report(start_report).await).is_err() {
            tracing::warn!(
                "start-repair before final stopped failed inline; queuing \
                 ordered pair for bounded background retry"
            );
            flush_queue().enqueue(sink.clone(), ctx.clone(), false, report);
            return;
        }
    }
    if (sink.report(report.clone()).await).is_err() {
        tracing::warn!("stopped report failed inline; queuing for bounded background retry");
        // Start is established by this point (acked earlier or repaired
        // just above), so the flush only needs to deliver the Stopped.
        flush_queue().enqueue(sink.clone(), ctx.clone(), true, report);
    }
}

impl Drop for ReportingSession {
    /// A session dropped without `.stop()` leaves its server-side
    /// `Sessions/Playing` state dangling; this at least logs it.
    fn drop(&mut self) {
        if !self.stopped {
            tracing::warn!(
                item_id = %self.ctx.item_id,
                play_session_id = %self.ctx.play_session_id,
                "ReportingSession dropped without calling stop() -- the final \
                 Stopped playback report was never sent for this session"
            );
        }
    }
}

fn build_report(
    ctx: &ReportContext,
    kind: PlaybackReportKind,
    position_ticks: i64,
    volume_level: u8,
    audio_stream_index: Option<i32>,
    subtitle_stream_index: Option<i32>,
) -> PlaybackReport {
    PlaybackReport {
        kind,
        item_id: ctx.item_id.clone(),
        media_source_id: ctx.media_source_id.clone(),
        position_ticks,
        is_paused: false,
        volume_level,
        audio_stream_index,
        subtitle_stream_index,
        play_session_id: ctx.play_session_id.clone(),
    }
}

/// The background actor: owns the authoritative playback state, the 10s
/// progress ticker, and immediate edge reports on pause/seek/track-change
/// (seek reports are debounced — see [`SEEK_DEBOUNCE`]).
async fn run(
    sink: Arc<dyn ReportSink>,
    ctx: ReportContext,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    start_acked: Arc<AtomicBool>,
) {
    let mut state = PlaybackState {
        position_ticks: 0,
        is_paused: false,
        audio_stream_index: None,
        subtitle_stream_index: None,
    };

    let start_ok = send_retrying(&sink, report_for(&ctx, &state, PlaybackReportKind::Start)).await;
    start_acked.store(start_ok, Ordering::Relaxed);

    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // first tick fires immediately; consume it

    // (Re)armed on each seek jump; see `SEEK_DEBOUNCE`.
    let mut seek_report_deadline: Option<tokio::time::Instant> = None;

    loop {
        let sleep_until_seek_deadline = tokio::time::sleep_until(
            seek_report_deadline.unwrap_or_else(tokio::time::Instant::now),
        );

        tokio::select! {
            _ = ticker.tick() => {
                send_retrying(&sink, report_for(&ctx, &state, PlaybackReportKind::Progress)).await;
            }
            () = sleep_until_seek_deadline, if seek_report_deadline.is_some() => {
                seek_report_deadline = None;
                send_retrying(&sink, report_for(&ctx, &state, PlaybackReportKind::Progress)).await;
                ticker.reset();
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(Cmd::Position(ticks)) => {
                        let jump = (ticks - state.position_ticks).abs();
                        state.position_ticks = ticks;
                        // >2s discontinuity is treated as a seek edge (no
                        // explicit on_seek exists in the frozen API).
                        if jump > seek_threshold_ticks() {
                            seek_report_deadline = Some(tokio::time::Instant::now() + SEEK_DEBOUNCE);
                        }
                    }
                    Some(Cmd::Pause(paused)) => {
                        if state.is_paused != paused {
                            state.is_paused = paused;
                            send_retrying(&sink, report_for(&ctx, &state, PlaybackReportKind::Progress)).await;
                            ticker.reset();
                        }
                    }
                    Some(Cmd::TrackChange(audio, subtitle)) => {
                        state.audio_stream_index = audio;
                        state.subtitle_stream_index = subtitle;
                        send_retrying(&sink, report_for(&ctx, &state, PlaybackReportKind::Progress)).await;
                        ticker.reset();
                    }
                    Some(Cmd::Stop { ticks, completion }) => {
                        send_final_stopped(
                            &sink,
                            &ctx,
                            start_acked.load(Ordering::Relaxed),
                            ticks,
                        ).await;
                        let _ = completion.send(());
                        break;
                    }
                    None => break,
                }
            }
        }
    }
}

fn seek_threshold_ticks() -> i64 {
    // Jellyfin ticks are 100ns units; 2s of drift is well beyond normal
    // on_position polling jitter but well inside a deliberate seek.
    2 * 10_000_000
}

fn report_for(
    ctx: &ReportContext,
    state: &PlaybackState,
    kind: PlaybackReportKind,
) -> PlaybackReport {
    PlaybackReport {
        kind,
        item_id: ctx.item_id.clone(),
        media_source_id: ctx.media_source_id.clone(),
        position_ticks: state.position_ticks,
        is_paused: state.is_paused,
        volume_level: DEFAULT_VOLUME,
        audio_stream_index: state.audio_stream_index,
        subtitle_stream_index: state.subtitle_stream_index,
        play_session_id: ctx.play_session_id.clone(),
    }
}

/// Bounded retry for start/progress reports — stale ones aren't worth
/// retrying forever since a fresher report follows within 10s. Returns
/// whether delivery succeeded, so `run()` can track the first Start (see
/// [`ReportingSession::start_acked`]).
async fn send_retrying(sink: &Arc<dyn ReportSink>, report: PlaybackReport) -> bool {
    let mut backoff = Backoff::new(RETRY_BASE, RETRY_CAP);
    for attempt in 0..TRANSIENT_REPORT_MAX_ATTEMPTS {
        match sink.report(report.clone()).await {
            Ok(()) => return true,
            Err(err) if attempt + 1 == TRANSIENT_REPORT_MAX_ATTEMPTS => {
                tracing::warn!(
                    ?err,
                    "giving up on playback report after {TRANSIENT_REPORT_MAX_ATTEMPTS} attempts"
                );
                return false;
            }
            Err(_) => tokio::time::sleep(backoff.next_delay()).await,
        }
    }
    false
}

/// Delivers exactly one background retry loop per in-flight `play_session_id`
/// for the one report that must never be silently dropped: the final
/// Stopped report. Without this, every failed inline report spawned its own
/// detached task with no cap on retry duration, accumulating without bound
/// across a long session. `FlushQueue` fixes both:
/// - **Caps retry duration**: gives up after [`FLUSH_MAX_DURATION`] and
///   logs a `tracing::error!` instead of retrying forever.
/// - **Dedups by `play_session_id`**: a second `enqueue` for a session
///   already in flight is dropped rather than spawning a competing task.
///
/// Intentionally a lazily-initialized process-wide singleton (see
/// [`flush_queue`]): `ReportingSession` has no natural shared owner across
/// sessions, and the dedup/cap guarantees only mean anything if every
/// `stop()` call funnels through the same queue.
struct FlushQueue {
    in_flight: Mutex<HashSet<String>>,
}

impl FlushQueue {
    fn new() -> Self {
        Self {
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Queue `report`'s delivery, unless a flush for the same
    /// `play_session_id` is already in flight (in which case this is a
    /// no-op — the in-flight attempt is already retrying, and it's already
    /// carrying the given `report`'s data since `stop()` is only ever
    /// called once per session). `start_acked` is whether this session's
    /// original `Start` report was ever delivered — `false` means the flush
    /// must resend one before `report` (see [`flush_with_cap`]).
    fn enqueue(
        &'static self,
        sink: Arc<dyn ReportSink>,
        ctx: ReportContext,
        start_acked: bool,
        report: PlaybackReport,
    ) {
        let session_id = report.play_session_id.clone();
        let already_in_flight = {
            let mut in_flight = self.in_flight.lock().expect("flush queue lock");
            !in_flight.insert(session_id.clone())
        };
        if already_in_flight {
            tracing::debug!(
                play_session_id = %session_id,
                "stopped-report flush already in-flight for this session; \
                 dropping duplicate enqueue rather than spawning a second task"
            );
            return;
        }

        tokio::spawn(async move {
            flush_with_cap(&sink, &ctx, start_acked, &report).await;
            self.in_flight
                .lock()
                .expect("flush queue lock")
                .remove(&session_id);
        });
    }
}

fn flush_queue() -> &'static FlushQueue {
    static QUEUE: OnceLock<FlushQueue> = OnceLock::new();
    QUEUE.get_or_init(FlushQueue::new)
}

/// Capped-backoff retry loop backing [`FlushQueue`]: retries until success
/// or until [`FLUSH_MAX_DURATION`] has elapsed, at which point it gives up
/// loudly (`tracing::error!`) instead of retrying forever.
///
/// If this session's `Start` report was never acknowledged
/// (`start_acked == false`, an outage spanning the entire session), a bare
/// `Stopped` report has no `Sessions/Playing` record to close -- Jellyfin
/// ignores a `Stopped` for a `play_session_id` it never saw a `Start` for.
/// So this resends `Start` first and only proceeds to `report` once that
/// succeeds, both under the same [`FLUSH_MAX_DURATION`] deadline.
async fn flush_with_cap(
    sink: &Arc<dyn ReportSink>,
    ctx: &ReportContext,
    start_acked: bool,
    report: &PlaybackReport,
) {
    let mut backoff = Backoff::new(RETRY_BASE, RETRY_CAP);
    let deadline = tokio::time::Instant::now() + FLUSH_MAX_DURATION;
    let mut start_delivered = start_acked;

    while !start_delivered {
        let start_report = build_report(
            ctx,
            PlaybackReportKind::Start,
            report.position_ticks,
            report.volume_level,
            report.audio_stream_index,
            report.subtitle_stream_index,
        );
        match sink.report(start_report).await {
            Ok(()) => start_delivered = true,
            Err(err) => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    tracing::error!(
                        ?err,
                        play_session_id = %report.play_session_id,
                        max_duration = ?FLUSH_MAX_DURATION,
                        "giving up on final Stopped playback report after the \
                         retry cap elapsed while re-sending the never-acked \
                         Start report; the server will never see this \
                         session's stop position"
                    );
                    return;
                }
                let delay = backoff.next_delay().min(deadline - now);
                tokio::time::sleep(delay).await;
            }
        }
    }

    loop {
        match sink.report(report.clone()).await {
            Ok(()) => return,
            Err(err) => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    tracing::error!(
                        ?err,
                        play_session_id = %report.play_session_id,
                        max_duration = ?FLUSH_MAX_DURATION,
                        "giving up on final Stopped playback report after the \
                         retry cap elapsed; the server will never see this \
                         session's stop position"
                    );
                    return;
                }
                let delay = backoff.next_delay().min(deadline - now);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct FakeSink {
        calls: Mutex<Vec<PlaybackReport>>,
        /// Number of leading calls to fail before succeeding forever after.
        fail_first: Mutex<u32>,
        notify: Notify,
    }

    impl FakeSink {
        fn new(fail_first: u32) -> Arc<Self> {
            Arc::new(Self {
                fail_first: Mutex::new(fail_first),
                ..Default::default()
            })
        }

        fn calls(&self) -> Vec<PlaybackReport> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl ReportSink for FakeSink {
        fn report(&self, report: PlaybackReport) -> BoxFuture<'_, Result<(), ApiError>> {
            Box::pin(async move {
                self.calls.lock().expect("lock").push(report);
                self.notify.notify_waiters();
                let mut remaining = self.fail_first.lock().expect("lock");
                if *remaining > 0 {
                    *remaining -= 1;
                    Err(ApiError::Transport("simulated failure".into()))
                } else {
                    Ok(())
                }
            })
        }
    }

    struct BlockingStartSink {
        calls: Mutex<Vec<PlaybackReport>>,
        start_entered: AtomicBool,
        release_start: Notify,
    }

    struct FirstRequestBlocksSink {
        calls: Mutex<Vec<PlaybackReport>>,
        request_count: std::sync::atomic::AtomicUsize,
    }

    impl FirstRequestBlocksSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                request_count: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> Vec<PlaybackReport> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl ReportSink for FirstRequestBlocksSink {
        fn report(&self, report: PlaybackReport) -> BoxFuture<'_, Result<(), ApiError>> {
            Box::pin(async move {
                self.calls.lock().expect("lock").push(report);
                if self.request_count.fetch_add(1, Ordering::AcqRel) == 0 {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    }

    impl BlockingStartSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                start_entered: AtomicBool::new(false),
                release_start: Notify::new(),
            })
        }

        fn calls(&self) -> Vec<PlaybackReport> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl ReportSink for BlockingStartSink {
        fn report(&self, report: PlaybackReport) -> BoxFuture<'_, Result<(), ApiError>> {
            Box::pin(async move {
                let is_start = matches!(report.kind, PlaybackReportKind::Start);
                self.calls.lock().expect("lock").push(report);
                if is_start {
                    self.start_entered.store(true, Ordering::Release);
                    self.release_start.notified().await;
                }
                Ok(())
            })
        }
    }

    fn ctx() -> ReportContext {
        ReportContext {
            item_id: "item".into(),
            media_source_id: "source".into(),
            play_session_id: "session".into(),
        }
    }

    /// Like [`ctx`], but with a caller-chosen `play_session_id`. Used by
    /// tests that exercise [`FlushQueue`] (a process-wide singleton keyed by
    /// `play_session_id`) so they can't collide with each other or with
    /// `stop_report_survives_transient_failures_via_background_retry`.
    fn ctx_with_session(play_session_id: &str) -> ReportContext {
        ReportContext {
            item_id: "item".into(),
            media_source_id: "source".into(),
            play_session_id: play_session_id.into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sends_start_report_immediately() {
        let sink = FakeSink::new(0);
        let session = ReportingSession::start_with_sink(sink.clone(), ctx());
        tokio::task::yield_now().await;

        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0].kind, PlaybackReportKind::Start));

        session.stop(0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn sends_progress_report_every_ten_seconds() {
        let sink = FakeSink::new(0);
        let session = ReportingSession::start_with_sink(sink.clone(), ctx());
        tokio::task::yield_now().await;
        assert_eq!(sink.calls().len(), 1); // start

        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(sink.calls().len(), 2);
        assert!(matches!(sink.calls()[1].kind, PlaybackReportKind::Progress));

        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(sink.calls().len(), 3);

        session.stop(0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn pause_sends_immediate_edge_report() {
        let sink = FakeSink::new(0);
        let mut session = ReportingSession::start_with_sink(sink.clone(), ctx());
        tokio::task::yield_now().await;
        assert_eq!(sink.calls().len(), 1);

        session.on_pause(true);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let calls = sink.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].is_paused);

        session.stop(0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn large_position_jump_is_reported_as_seek_edge_after_debounce_quiet_window() {
        let sink = FakeSink::new(0);
        let mut session = ReportingSession::start_with_sink(sink.clone(), ctx());
        tokio::task::yield_now().await;
        assert_eq!(sink.calls().len(), 1);

        // 30s jump (Jellyfin ticks are 100ns units).
        session.on_position(30 * 10_000_000);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // Not reported yet -- a seek report only fires once
        // position updates go quiet for SEEK_DEBOUNCE (500ms), so a single
        // jump alone must not produce an immediate report.
        assert_eq!(
            sink.calls().len(),
            1,
            "seek report must be debounced, not sent immediately"
        );

        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let calls = sink.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].position_ticks, 30 * 10_000_000);

        session.stop(0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn small_position_updates_do_not_spam_reports() {
        let sink = FakeSink::new(0);
        let mut session = ReportingSession::start_with_sink(sink.clone(), ctx());
        tokio::task::yield_now().await;

        session.on_position(1_000_000); // 100ms — not a seek
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
        assert_eq!(sink.calls().len(), 1); // no edge report fired

        session.stop(0).await;
    }

    /// Pins: a scrub-bar drag's rapid large jumps coalesce into exactly one debounced report, reflecting the final position.
    #[tokio::test(start_paused = true)]
    async fn rapid_scrub_storm_coalesces_into_a_single_debounced_report() {
        let sink = FakeSink::new(0);
        let mut session = ReportingSession::start_with_sink(sink.clone(), ctx());
        tokio::task::yield_now().await;
        assert_eq!(sink.calls().len(), 1);

        // Several large jumps in quick succession, each well inside the
        // 500ms debounce window of the previous one -- simulates dragging
        // a scrub bar around.
        for seconds in [10_i64, 20, 5, 40, 33] {
            session.on_position(seconds * 10_000_000);
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sink.calls().len(),
            1,
            "a burst of seeks within the debounce window must not produce a report per jump"
        );

        // Let the window finally go quiet.
        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let calls = sink.calls();
        assert_eq!(
            calls.len(),
            2,
            "the whole scrub burst must coalesce into exactly one report"
        );
        assert_eq!(
            calls[1].position_ticks,
            33 * 10_000_000,
            "the coalesced report must reflect the final position, not an intermediate one"
        );

        session.stop(0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn track_change_sends_immediate_edge_report() {
        let sink = FakeSink::new(0);
        let mut session = ReportingSession::start_with_sink(sink.clone(), ctx());
        tokio::task::yield_now().await;

        session.on_track_change(Some(2), Some(3));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let calls = sink.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].audio_stream_index, Some(2));
        assert_eq!(calls[1].subtitle_stream_index, Some(3));

        session.stop(0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn stop_always_sends_final_stopped_report() {
        let sink = FakeSink::new(0);
        let session = ReportingSession::start_with_sink(sink.clone(), ctx());
        tokio::task::yield_now().await;

        session.stop(12345).await;
        let calls = sink.calls();
        let last = calls.last().expect("test assertion");
        assert!(matches!(last.kind, PlaybackReportKind::Stopped));
        assert_eq!(last.position_ticks, 12345);
    }

    #[tokio::test]
    async fn stop_waits_for_start_before_sending_stopped() {
        let sink = BlockingStartSink::new();
        let session = ReportingSession::start_with_sink(sink.clone(), ctx());
        while !sink.start_entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }

        let stop_task = tokio::spawn(async move { session.stop(42).await });
        tokio::task::yield_now().await;
        assert!(
            sink.calls()
                .iter()
                .all(|r| matches!(r.kind, PlaybackReportKind::Start)),
            "Stopped must not overtake an in-flight Start report"
        );

        // `notify_one` retains a permit if the actor has not yet polled its
        // wait future, unlike `notify_waiters`.
        sink.release_start.notify_one();
        stop_task.await.expect("stop task");
        let calls = sink.calls();
        assert!(matches!(calls.as_slice(), [start, stopped]
            if matches!(start.kind, PlaybackReportKind::Start)
                && matches!(stopped.kind, PlaybackReportKind::Stopped)));
        assert_eq!(calls[1].position_ticks, 42);
    }

    #[tokio::test]
    async fn stop_timeout_hands_final_report_to_flush_queue() {
        let sink = FirstRequestBlocksSink::new();
        let session = ReportingSession::start_with_sink(sink.clone(), ctx_with_session("timeout"));
        while sink.calls().is_empty() {
            tokio::task::yield_now().await;
        }

        session.stop(777).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let calls = sink.calls();
        let stopped = calls
            .iter()
            .find(|report| matches!(report.kind, PlaybackReportKind::Stopped))
            .expect("timeout must enqueue a final Stopped report");
        assert_eq!(stopped.position_ticks, 777);
        let stopped_index = calls
            .iter()
            .position(|report| matches!(report.kind, PlaybackReportKind::Stopped))
            .expect("stopped index");
        assert!(
            calls[..stopped_index]
                .iter()
                .any(|report| matches!(report.kind, PlaybackReportKind::Start)),
            "FlushQueue must establish Start before final Stopped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_reestablishes_never_acked_start_before_inline_stopped() {
        // F7: Start exhausts all its attempts during an outage, then the
        // server recovers. The actor's *inline* final delivery must send
        // Start before Stopped -- a bare Stopped for a play_session_id the
        // server never saw a Start for is ignored, silently losing the
        // session's watch state exactly when the inline send succeeds (so
        // the FlushQueue's own repair would never run).
        let sink = FakeSink::new(TRANSIENT_REPORT_MAX_ATTEMPTS);
        let session = ReportingSession::start_with_sink(
            sink.clone(),
            ctx_with_session("start-repair-inline"),
        );
        // Drive through the failed Start attempts and their 1/2/4/8s
        // backoffs (15s total).
        for _ in 0..8 {
            tokio::time::advance(Duration::from_secs(4)).await;
            tokio::task::yield_now().await;
        }
        assert!(
            !session.start_acked.load(Ordering::Relaxed),
            "precondition: the initial Start must have given up unacked"
        );

        session.stop(555).await;

        let calls = sink.calls();
        let stopped_index = calls
            .iter()
            .position(|r| matches!(r.kind, PlaybackReportKind::Stopped))
            .expect("inline Stopped must be delivered");
        assert_eq!(calls[stopped_index].position_ticks, 555);
        assert!(
            stopped_index >= 1
                && matches!(calls[stopped_index - 1].kind, PlaybackReportKind::Start),
            "a never-acked Start must be re-established immediately before the inline Stopped"
        );
        assert_eq!(
            calls.len(),
            stopped_index + 1,
            "inline delivery succeeded, so no flush retry may add further reports"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_report_survives_transient_failures_via_background_retry() {
        // Fails the first 3 attempts, then succeeds — stop() must still
        // guarantee delivery via the FlushQueue's retry task.
        let sink = FakeSink::new(3);
        let session =
            ReportingSession::start_with_sink(sink.clone(), ctx_with_session("bg-retry-session"));
        tokio::task::yield_now().await;

        // The actor is asleep in its Start-retry backoff, so stop() times
        // out at STOP_ACK_TIMEOUT, aborts it, and enqueues onto the
        // FlushQueue -- which must deliver despite the remaining failures.
        session.stop(999).await;

        // Advance past several backoff cycles to let the retry task succeed.
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(70)).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }

        let calls = sink.calls();
        let stopped: Vec<_> = calls
            .iter()
            .filter(|r| matches!(r.kind, PlaybackReportKind::Stopped))
            .collect();
        assert!(
            !stopped.is_empty(),
            "stopped report must eventually be delivered"
        );
        assert_eq!(stopped.last().expect("test assertion").position_ticks, 999);
    }

    // --- FlushQueue -------------------------------------------

    /// A `ReportSink` whose `report()` call blocks on a `Notify` until the
    /// test explicitly releases it — used to force two enqueued flushes for
    /// the same session to genuinely overlap in time, rather than the first
    /// completing before the second is even enqueued.
    #[derive(Default)]
    struct GatedSink {
        calls: Mutex<Vec<PlaybackReport>>,
        gate: Notify,
    }

    impl ReportSink for GatedSink {
        fn report(&self, report: PlaybackReport) -> BoxFuture<'_, Result<(), ApiError>> {
            Box::pin(async move {
                self.calls.lock().expect("lock").push(report);
                self.gate.notified().await;
                Ok(())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn flush_queue_dedups_a_second_enqueue_for_a_session_already_in_flight() {
        let sink = Arc::new(GatedSink::default());
        let ctx = ctx_with_session("dedup-session");
        let report = build_report(
            &ctx,
            PlaybackReportKind::Stopped,
            42,
            DEFAULT_VOLUME,
            None,
            None,
        );

        // `start_acked: true` -- this test isn't exercising the
        // never-acked-Start resend path (see `flush_resends_start_report_...`
        // below), so it must not add an extra `report()` call.
        flush_queue().enqueue(sink.clone(), ctx.clone(), true, report.clone());
        tokio::task::yield_now().await;
        // A second enqueue for the SAME session while the first is still
        // in flight (blocked on the gate) must be dropped, not spawn a
        // second concurrent retry task.
        flush_queue().enqueue(sink.clone(), ctx.clone(), true, report.clone());
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert_eq!(
            sink.calls.lock().expect("lock").len(),
            1,
            "a duplicate enqueue for an in-flight session must not spawn a second task"
        );

        // Let the one in-flight attempt finish so the runtime shuts down
        // cleanly.
        sink.gate.notify_waiters();
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn flush_queue_allows_concurrent_flushes_for_different_sessions() {
        let sink = Arc::new(GatedSink::default());
        let ctx_a = ctx_with_session("session-a");
        let ctx_b = ctx_with_session("session-b");
        let report_a = build_report(
            &ctx_a,
            PlaybackReportKind::Stopped,
            1,
            DEFAULT_VOLUME,
            None,
            None,
        );
        let report_b = build_report(
            &ctx_b,
            PlaybackReportKind::Stopped,
            2,
            DEFAULT_VOLUME,
            None,
            None,
        );

        flush_queue().enqueue(sink.clone(), ctx_a, true, report_a);
        flush_queue().enqueue(sink.clone(), ctx_b, true, report_b);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Distinct sessions are NOT deduped against each other -- both
        // should be in flight (both blocked on the shared gate).
        assert_eq!(
            sink.calls.lock().expect("lock").len(),
            2,
            "different play_session_ids must be able to flush concurrently"
        );

        sink.gate.notify_waiters();
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn flush_queue_gives_up_after_the_retry_cap_instead_of_retrying_forever() {
        // Never succeeds -- this is the "server is permanently unreachable"
        // case the cap exists for.
        let sink = FakeSink::new(u32::MAX);
        let ctx = ctx_with_session("cap-test-session");
        let report = build_report(
            &ctx,
            PlaybackReportKind::Stopped,
            7,
            DEFAULT_VOLUME,
            None,
            None,
        );
        flush_queue().enqueue(sink.clone(), ctx, true, report);

        // Advance well past FLUSH_MAX_DURATION (30 minutes).
        for _ in 0..40 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        let calls_past_cap = sink.calls().len();
        assert!(
            calls_past_cap > 0,
            "expected at least one retry attempt before the cap"
        );

        // Advance well beyond that again -- the call count must NOT keep
        // growing once the task has given up (pre-fix, `flush_forever` had
        // no cap and would still be retrying here, forever).
        for _ in 0..15 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sink.calls().len(),
            calls_past_cap,
            "flush must give up after FLUSH_MAX_DURATION instead of retrying forever"
        );
    }

    /// Pins: when an outage spans the entire session (Start and the inline
    /// Stopped both fail before it's queued), the flush notices
    /// `start_acked == false` and resends `Start` before `Stopped` once the
    /// server recovers -- otherwise Jellyfin ignores a bare `Stopped`.
    #[tokio::test(start_paused = true)]
    async fn flush_resends_start_report_before_stopped_when_start_was_never_acked() {
        let sink = FakeSink::new(6);
        let session = ReportingSession::start_with_sink(
            sink.clone(),
            ctx_with_session("full-outage-session"),
        );

        // Let `run()`'s bounded Start retries (5 attempts, capped backoff)
        // burn through their failures -- advancing in small (1s) steps and
        // stopping the instant the 5th call lands, rather than jumping far
        // past it, so the clock never reaches the 10s-after-Start-finishes
        // mark where the progress ticker would also fire and add an
        // unrelated Progress call to the count below.
        for _ in 0..60 {
            tokio::task::yield_now().await;
            if sink.calls().len() >= 5 {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        tokio::task::yield_now().await;
        assert_eq!(
            sink.calls().len(),
            5,
            "expected exactly the 5 bounded Start attempts from run() so far"
        );

        // The actor's inline delivery now repairs the never-acked Start
        // FIRST (see `send_final_stopped`): that repair attempt is the 6th
        // call, also fails, and the ordered pair is enqueued onto the
        // FlushQueue without a bare Stopped ever going out first.
        session.stop(555).await;

        // Let the FlushQueue's background retry run -- every call from here
        // on succeeds (fail_first is exhausted).
        for _ in 0..10 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }

        let calls = sink.calls();
        let start_calls: Vec<_> = calls
            .iter()
            .filter(|r| matches!(r.kind, PlaybackReportKind::Start))
            .collect();
        let stopped_calls: Vec<_> = calls
            .iter()
            .filter(|r| matches!(r.kind, PlaybackReportKind::Stopped))
            .collect();

        assert_eq!(
            start_calls.len(),
            7,
            "expected 5 failed Start attempts from run(), 1 failed inline \
             Start repair, and 1 resent Start from the flush, got calls={calls:?}"
        );
        assert_eq!(
            stopped_calls.len(),
            1,
            "a bare Stopped must never be attempted while Start is unacked; \
             only the flush's ordered Stopped should appear, got calls={calls:?}"
        );
        assert_eq!(
            stopped_calls.last().expect("test assertion").position_ticks,
            555,
            "the delivered Stopped report must carry the real stop position"
        );
        assert!(
            matches!(
                calls.last().expect("test assertion").kind,
                PlaybackReportKind::Stopped
            ),
            "the resent Start must be sent (and succeed) before the final \
             Stopped report, got calls={calls:?}"
        );
    }

    // --- must_use + Drop warning -------------------------------

    /// Minimal `tracing::Subscriber` that just captures event messages, so
    /// tests can assert a `tracing::warn!`/`error!` actually fired without
    /// pulling in a test-capture crate as a new dependency.
    #[derive(Clone)]
    struct CapturingSubscriber {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct MessageVisitor(String);
            impl tracing::field::Visit for MessageVisitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            self.messages.lock().expect("lock").push(visitor.0);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_without_stop_logs_a_warning() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            messages: messages.clone(),
        };
        let sink = FakeSink::new(0);

        tracing::subscriber::with_default(subscriber, || {
            let session = ReportingSession::start_with_sink(
                sink.clone(),
                ctx_with_session("dropped-session"),
            );
            drop(session); // never called .stop()
        });

        let logged = messages.lock().expect("lock");
        assert!(
            logged
                .iter()
                .any(|m| m.contains("dropped without calling stop")),
            "expected a warning that the session was dropped without stop(), got {logged:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_suppresses_the_drop_warning() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            messages: messages.clone(),
        };
        let sink = FakeSink::new(0);

        let session =
            ReportingSession::start_with_sink(sink.clone(), ctx_with_session("properly-stopped"));
        tokio::task::yield_now().await;

        // `with_default` only accepts a synchronous closure, but `Drop`
        // runs wherever `session` (moved into `.stop()`) is finally
        // released, which happens at the end of the `.await`ed future --
        // `set_default` instead installs the subscriber as this (current-
        // thread runtime) thread's default until the guard drops, so it's
        // still active whenever that happens.
        let _guard = tracing::subscriber::set_default(subscriber);
        session.stop(0).await; // properly stopped -- must NOT warn on drop
        drop(_guard);

        let logged = messages.lock().expect("lock");
        assert!(
            !logged
                .iter()
                .any(|m| m.contains("dropped without calling stop")),
            "stop() must suppress the drop-without-stop warning, got {logged:?}"
        );
    }
}
