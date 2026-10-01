//! Supervised WebSocket: owns reconnect/backoff and emits both server events
//! and connection-state transitions.
//!
//! The supervision loop ([`supervise`]) is generic over a private
//! [`WsConnect`] trait so tests can drive it against a scripted fake
//! connector with `tokio::time::pause` + `advance`, skipping backoff delays
//! deterministically instead of exercising a real WebSocket.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use jellyfin_api::{ApiError, JellyfinClient, ServerEvent};
use tokio::sync::{broadcast, mpsc};

use crate::backoff::Backoff;
use crate::BusEvent;

const RECONNECT_BASE: Duration = Duration::from_secs(1);
const RECONNECT_CAP: Duration = Duration::from_secs(60);
const BROADCAST_CAPACITY: usize = 256;

/// A connection only "proves useful" -- and earns a backoff reset -- once
/// it's held for this long or delivered a real server event, whichever
/// comes first. Otherwise a server that accepts the handshake and then
/// immediately drops the connection would pin backoff at ~1s forever
/// instead of escalating.
const USEFUL_CONNECTION_DURATION: Duration = Duration::from_secs(30);

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait WsConnect: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'_, Result<mpsc::Receiver<ServerEvent>, ApiError>>;
}

impl WsConnect for JellyfinClient {
    fn connect(&self) -> BoxFuture<'_, Result<mpsc::Receiver<ServerEvent>, ApiError>> {
        Box::pin(self.connect_ws())
    }
}

pub struct EventBus {
    _priv: (),
}

impl EventBus {
    /// Spawn the supervisor. The returned [`EventBusHandle`] must be kept
    /// alive for as long as the bus should keep running; dropping it (or
    /// calling [`EventBusHandle::shutdown`] explicitly) terminates the
    /// supervision loop and closes any live WebSocket connection.
    pub fn spawn(client: JellyfinClient) -> (broadcast::Receiver<BusEvent>, EventBusHandle) {
        let (tx, rx) = broadcast::channel(BROADCAST_CAPACITY);
        let task = tokio::spawn(supervise(client, tx.clone()));
        (
            rx,
            EventBusHandle {
                task: Some(task),
                tx,
            },
        )
    }
}

/// Handle controlling the lifetime of a spawned [`EventBus`] supervisor.
/// Without it, every reconnect/server-switch would leak a supervisor task
/// retrying forever with no way to stop it short of process exit.
pub struct EventBusHandle {
    task: Option<tokio::task::JoinHandle<()>>,
    /// Kept so `subscribe()` can hand out additional independent receivers
    /// after the fact -- `EventBus::spawn`'s original receiver is one-shot.
    tx: broadcast::Sender<BusEvent>,
}

impl EventBusHandle {
    /// Hands out a fresh, independent [`broadcast::Receiver`]; broadcast
    /// semantics mean it only misses events sent before it was created.
    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }

    /// Stop the supervision loop and wait for it to fully exit. Aborting
    /// the task drops any live connection receiver, which `ws.rs`'s
    /// `tx.closed()` race turns into a prompt WebSocket `Close` frame; the
    /// join then guarantees every resource is released before returning.
    ///
    /// Prefer this over dropping the handle when the caller can await;
    /// [`Drop`] does the same abort as a best-effort fallback.
    pub async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for EventBusHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// The supervision loop: connect, forward events until the connection drops
/// (channel closes) or fails outright, emit connection-state transitions
/// around both, then reconnect with jittered capped backoff. Runs until
/// [`EventBusHandle::shutdown`]/[`Drop`] aborts the task that owns this
/// future.
async fn supervise<C: WsConnect>(connector: C, tx: broadcast::Sender<BusEvent>) {
    let mut backoff = Backoff::new(RECONNECT_BASE, RECONNECT_CAP);
    loop {
        match connector.connect().await {
            Ok(mut rx) => {
                let _ = tx.send(BusEvent::Connected);
                // Every successful (re)connect may have missed events while
                // we were down -- the caller (media-cache) must reconcile.
                let _ = tx.send(BusEvent::NeedsReconcile);

                // See USEFUL_CONNECTION_DURATION for the reset condition;
                // until met, backoff keeps escalating rather than resetting.
                let proved_useful_at = tokio::time::sleep(USEFUL_CONNECTION_DURATION);
                tokio::pin!(proved_useful_at);
                let mut proved_useful = false;

                loop {
                    tokio::select! {
                        biased;
                        () = &mut proved_useful_at, if !proved_useful => {
                            proved_useful = true;
                            backoff.reset();
                        }
                        event = rx.recv() => {
                            match event {
                                Some(event) => {
                                    if !proved_useful {
                                        proved_useful = true;
                                        backoff.reset();
                                    }
                                    let _ = tx.send(BusEvent::Server(event));
                                }
                                None => break,
                            }
                        }
                    }
                }
                let _ = tx.send(BusEvent::Disconnected);
            }
            Err(err) => {
                tracing::warn!(?err, "websocket connect failed, will retry with backoff");
                let _ = tx.send(BusEvent::Disconnected);
            }
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    enum Outcome {
        Ok(Vec<ServerEvent>),
        Err,
    }

    struct FakeConnector {
        outcomes: Mutex<VecDeque<Outcome>>,
    }

    impl WsConnect for FakeConnector {
        fn connect(&self) -> BoxFuture<'_, Result<mpsc::Receiver<ServerEvent>, ApiError>> {
            Box::pin(async move {
                let outcome = self.outcomes.lock().expect("lock").pop_front();
                match outcome {
                    Some(Outcome::Ok(events)) => {
                        let (tx, rx) = mpsc::channel(16);
                        tokio::spawn(async move {
                            for event in events {
                                if tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                            // tx dropped here: rx.recv() -> None.
                        });
                        Ok(rx)
                    }
                    Some(Outcome::Err) | None => Err(ApiError::Transport("simulated".into())),
                }
            })
        }
    }

    /// Connects successfully but never proves useful: the sender is dropped
    /// immediately, closing the connection with no events sent. Records the
    /// (paused, virtual) `Instant` of every `connect()` call so tests can
    /// inspect the actual reconnect cadence.
    struct FlappingState {
        call_times: Mutex<Vec<tokio::time::Instant>>,
    }

    struct FlappingConnector(Arc<FlappingState>);

    impl WsConnect for FlappingConnector {
        fn connect(&self) -> BoxFuture<'_, Result<mpsc::Receiver<ServerEvent>, ApiError>> {
            let state = self.0.clone();
            Box::pin(async move {
                state
                    .call_times
                    .lock()
                    .expect("lock")
                    .push(tokio::time::Instant::now());
                let (_tx, rx) = mpsc::channel::<ServerEvent>(1);
                // `_tx` drops here -> `rx.recv()` immediately yields `None`.
                Ok(rx)
            })
        }
    }

    fn is_server_force_keep_alive(ev: &BusEvent) -> bool {
        matches!(ev, BusEvent::Server(ServerEvent::ForceKeepAlive))
    }

    /// Drains whatever the supervision loop has produced so far, jumping
    /// the paused clock 65s per round (past the 60s backoff cap) to unstick
    /// sleeps. Only safe for asserting event order/type, never backoff
    /// timing -- a jump always past the cap can't distinguish "reset" from
    /// "escalated". Tests that check timing step the clock finely instead.
    async fn drain(rx: &mut broadcast::Receiver<BusEvent>, rounds: u32) -> Vec<BusEvent> {
        let mut events = Vec::new();
        for _ in 0..rounds {
            tokio::task::yield_now().await;
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
            tokio::time::advance(Duration::from_secs(65)).await;
        }
        tokio::task::yield_now().await;
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_sequence_emits_expected_status_and_server_events() {
        let connector = FakeConnector {
            outcomes: Mutex::new(VecDeque::from([
                Outcome::Err,
                Outcome::Ok(vec![ServerEvent::ForceKeepAlive]),
                Outcome::Err,
                Outcome::Ok(vec![]),
            ])),
        };
        let (tx, mut rx) = broadcast::channel(256);
        tokio::spawn(supervise(connector, tx));

        let events = drain(&mut rx, 12).await;

        assert!(
            events.len() >= 9,
            "expected at least 9 events, got {events:?}"
        );
        assert!(
            matches!(events[0], BusEvent::Disconnected),
            "{:?}",
            events[0]
        );
        assert!(matches!(events[1], BusEvent::Connected), "{:?}", events[1]);
        assert!(
            matches!(events[2], BusEvent::NeedsReconcile),
            "{:?}",
            events[2]
        );
        assert!(is_server_force_keep_alive(&events[3]), "{:?}", events[3]);
        assert!(
            matches!(events[4], BusEvent::Disconnected),
            "{:?}",
            events[4]
        );
        assert!(
            matches!(events[5], BusEvent::Disconnected),
            "{:?}",
            events[5]
        );
        assert!(matches!(events[6], BusEvent::Connected), "{:?}", events[6]);
        assert!(
            matches!(events[7], BusEvent::NeedsReconcile),
            "{:?}",
            events[7]
        );
        assert!(
            matches!(events[8], BusEvent::Disconnected),
            "{:?}",
            events[8]
        );
    }

    /// Pins: `NeedsReconcile` fires on every reconnect, and backoff
    /// escalates (not resets) across repeated flapping reconnects.
    #[tokio::test(start_paused = true)]
    async fn needs_reconcile_fires_on_every_successful_reconnect_and_backoff_still_escalates() {
        let state = Arc::new(FlappingState {
            call_times: Mutex::new(Vec::new()),
        });
        let (tx, mut rx) = broadcast::channel(256);
        tokio::spawn(supervise(FlappingConnector(state.clone()), tx));

        // Fine-grained stepping (not `drain`'s 65s jump) so the recorded
        // `Instant`s reflect the true reconnect cadence.
        for _ in 0..200 {
            tokio::time::advance(Duration::from_millis(250)).await;
            tokio::task::yield_now().await;
        }

        let mut reconcile_count = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, BusEvent::NeedsReconcile) {
                reconcile_count += 1;
            }
        }
        assert!(
            reconcile_count >= 3,
            "expected NeedsReconcile on every reconnect (>=3 in this window), got {reconcile_count}"
        );

        let calls = state.call_times.lock().expect("lock").clone();
        assert!(
            calls.len() >= 4,
            "expected several reconnect attempts, got {}",
            calls.len()
        );
        let gaps: Vec<Duration> = calls.windows(2).map(|w| w[1] - w[0]).collect();
        for i in 1..gaps.len() {
            assert!(
                gaps[i] > gaps[i - 1],
                "backoff must escalate, not reset, across repeated flapping \
                 reconnects (this is the exact bug M4 fixes): gaps={gaps:?}"
            );
        }
    }

    /// Pins: backoff grows monotonically and reaches well past its 1s base
    /// under sustained flapping reconnects, not just "not exactly 1s".
    #[tokio::test(start_paused = true)]
    async fn backoff_escalates_on_repeated_flapping_reconnects() {
        let state = Arc::new(FlappingState {
            call_times: Mutex::new(Vec::new()),
        });
        let (tx, _rx) = broadcast::channel(256);
        tokio::spawn(supervise(FlappingConnector(state.clone()), tx));

        for _ in 0..400 {
            tokio::time::advance(Duration::from_millis(250)).await;
            tokio::task::yield_now().await;
        }

        let calls = state.call_times.lock().expect("lock").clone();
        assert!(
            calls.len() >= 6,
            "expected multiple reconnect attempts, got {}",
            calls.len()
        );
        let gaps: Vec<Duration> = calls.windows(2).map(|w| w[1] - w[0]).collect();
        for i in 1..gaps.len() {
            assert!(
                gaps[i] > gaps[i - 1],
                "backoff must monotonically escalate on flapping reconnects: gaps={gaps:?}"
            );
        }
        assert!(
            gaps.last().expect("at least one gap").as_secs_f64() > 5.0,
            "expected backoff to escalate well past its 1s base by the last \
             recorded gap, gaps={gaps:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn keeps_retrying_forever_after_repeated_failures() {
        let connector = FakeConnector {
            outcomes: Mutex::new(VecDeque::new()),
        };
        let (tx, mut rx) = broadcast::channel(256);
        tokio::spawn(supervise(connector, tx));

        let events = drain(&mut rx, 10).await;
        assert!(events.len() >= 5);
        assert!(events.iter().all(|e| matches!(e, BusEvent::Disconnected)));
    }

    /// Pins: `shutdown()` aborts the loop and the task actually finishes,
    /// proven by the owned `broadcast::Sender` dropping and closing every
    /// receiver.
    #[tokio::test(start_paused = true)]
    async fn shutdown_terminates_the_supervision_loop_and_closes_the_broadcast_channel() {
        let connector = FakeConnector {
            outcomes: Mutex::new(VecDeque::new()), // always Err -> retries forever until stopped
        };
        let (tx, mut rx) = broadcast::channel(256);
        let task = tokio::spawn(supervise(connector, tx.clone()));
        let handle = EventBusHandle {
            task: Some(task),
            tx,
        };

        tokio::task::yield_now().await;
        while rx.try_recv().is_ok() {}

        handle.shutdown().await;

        match rx.recv().await {
            Err(broadcast::error::RecvError::Closed) => {}
            other => {
                panic!("expected the broadcast channel to close after shutdown, got {other:?}")
            }
        }
    }

    /// Pins: `shutdown()` also drops any live connection's receiver, not
    /// just stops reconnecting after failure -- `ws.rs`'s `tx.closed()`
    /// race turns that drop into a prompt WebSocket `Close`.
    #[tokio::test(start_paused = true)]
    async fn shutdown_drops_the_live_connection_promptly() {
        struct HangingState {
            sender: Mutex<Option<mpsc::Sender<ServerEvent>>>,
        }
        struct HangingConnector(Arc<HangingState>);
        impl WsConnect for HangingConnector {
            fn connect(&self) -> BoxFuture<'_, Result<mpsc::Receiver<ServerEvent>, ApiError>> {
                let state = self.0.clone();
                Box::pin(async move {
                    let (tx, rx) = mpsc::channel(16);
                    *state.sender.lock().expect("lock") = Some(tx);
                    Ok(rx)
                })
            }
        }

        let state = Arc::new(HangingState {
            sender: Mutex::new(None),
        });
        let (tx, _rx) = broadcast::channel(256);
        let task = tokio::spawn(supervise(HangingConnector(state.clone()), tx.clone()));
        let handle = EventBusHandle {
            task: Some(task),
            tx,
        };

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let sender = state
            .sender
            .lock()
            .expect("lock")
            .clone()
            .expect("connector should have connected by now");
        assert!(!sender.is_closed(), "sanity: not yet closed pre-shutdown");

        handle.shutdown().await;

        assert!(
            sender.is_closed(),
            "expected the live connection's receiver to be dropped (closing \
             any live WS) once shutdown() returns"
        );
    }

    /// Pins: dropping the handle (the fallback for callers that can't
    /// await `shutdown()`) still terminates the loop via abort.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_handle_also_terminates_the_supervision_loop() {
        let connector = FakeConnector {
            outcomes: Mutex::new(VecDeque::new()),
        };
        let (tx, mut rx) = broadcast::channel(256);
        let task = tokio::spawn(supervise(connector, tx.clone()));
        let handle = EventBusHandle {
            task: Some(task),
            tx,
        };

        tokio::task::yield_now().await;
        while rx.try_recv().is_ok() {}

        drop(handle);
        // Give the abort a scheduler tick to take effect.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        match rx.recv().await {
            Err(broadcast::error::RecvError::Closed) => {}
            other => panic!("expected the broadcast channel to close after drop, got {other:?}"),
        }
    }

    /// Pins: `subscribe()` hands out an independent receiver that sees the
    /// same events as the original one `EventBus::spawn` returned.
    #[tokio::test(start_paused = true)]
    async fn subscribe_hands_out_an_independent_receiver_seeing_the_same_events() {
        let connector = FakeConnector {
            outcomes: Mutex::new(VecDeque::from([Outcome::Ok(vec![
                ServerEvent::ForceKeepAlive,
            ])])),
        };
        let (tx, mut original_rx) = broadcast::channel(256);
        let task = tokio::spawn(supervise(connector, tx.clone()));
        let handle = EventBusHandle {
            task: Some(task),
            tx,
        };

        let mut second_rx = handle.subscribe();

        let original_events = drain(&mut original_rx, 3).await;
        let second_events = drain(&mut second_rx, 0).await; // already advanced by the drain above

        assert!(
            original_events
                .iter()
                .any(is_server_force_keep_alive),
            "sanity: original receiver should see the ForceKeepAlive event, got {original_events:?}"
        );
        // No `PartialEq` on `BusEvent`/`ServerEvent`; compare via `Debug`
        // formatting instead.
        assert_eq!(
            format!("{original_events:?}"),
            format!("{second_events:?}"),
            "a receiver from subscribe() must see the exact same events as the original"
        );

        handle.shutdown().await;
    }
}
