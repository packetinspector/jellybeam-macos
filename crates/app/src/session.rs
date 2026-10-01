//! Connect/resume flow: turns (server, username, password) or a stored
//! Keychain session into a live `JellyfinClient` + `Mirror` + `EventBus`.
//! Runs entirely on the tokio runtime, never on GPUI's main thread --
//! `main.rs`/`root.rs` bridge the result back via a oneshot channel.
//!
//! docs/OVERVIEW.md §6: every bundle is scoped to one `(base_url, user_id)` pair via
//! [`server_scope_dir`], a stable hash of the pair (not raw strings) so a
//! filesystem-unsafe URL/username can't produce a bad path component. This
//! also isolates per-user state (played state, resume points) when two
//! users share one server's mirror.

use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui::{Context, SharedString};
use jellyfin_api::{ClientIdentity, JellyfinClient};
use jellyfin_core::EventBusHandle;
use media_cache::{ImageCache, Mirror, Sort};

use crate::keychain::StoredSession;
use crate::root::{
    connect_error_line, main_state_from_bundle, new_connect_state, new_connect_state_connecting,
    transport_error_hint_for_api_error, transport_error_hint_for_message, MainState,
    PreloadTrigger, Root, Screen,
};

fn support_root() -> PathBuf {
    crate::paths::state_root()
}

fn caches_root() -> PathBuf {
    crate::paths::cache_root()
}

/// A stable, filesystem-safe directory-name component for one `(base_url,
/// user_id)` pair -- two independently-salted `DefaultHasher` outputs
/// concatenated, same approach as `keychain::device_id`'s `generate_id`.
/// Must use `DefaultHasher` (fixed initial state), not
/// `RandomState::new()`, which randomizes per call and would silently
/// break per-server mirror/cache directory lookup across launches.
fn server_scope_dir(base_url: &str, user_id: &str) -> String {
    let key = (base_url, user_id);
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&("jellybeam-scope-1", key), &mut h1);
    // Second, differently-salted hasher: doubles collision resistance for
    // a cache-directory name, not a security boundary.
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&("jellybeam-scope-2", key), &mut h2);
    format!("{:016x}{:016x}", h1.finish(), h2.finish())
}

/// Where one server+user's SQLite mirror lives: `dir =
/// app_support_dir()/<server_scope_dir>/` (see module doc).
pub(crate) fn app_support_dir_for(base_url: &str, user_id: &str) -> PathBuf {
    support_root().join(server_scope_dir(base_url, user_id))
}

/// Where one server+user's on-disk image cache lives (docs/DATA.md §3:
/// `Caches/Jellybeam/images/<server_scope_dir>/...`). Distinct from the
/// mirror's Application Support directory: this is disposable/re-fetchable,
/// the mirror isn't.
pub(crate) fn image_cache_dir_for(base_url: &str, user_id: &str) -> PathBuf {
    caches_root()
        .join("images")
        .join(server_scope_dir(base_url, user_id))
}

pub(crate) struct ConnectedBundle {
    pub client: JellyfinClient,
    pub mirror: Mirror,
    pub bus_handle: EventBusHandle,
    pub views: Vec<media_cache::ViewSummary>,
    pub image_cache: ImageCache,
    /// docs/UX-SPEC.md §6: true when assembled without a live round trip
    /// (`resume_flow`'s cache-first fallback only). Gates the offline
    /// banner and playback until `EventBus` reports `Connected`.
    pub offline: bool,
}

/// How long `resume_flow`'s live `/UserViews` sanity check waits before
/// treating the server as unreachable. Bounded so a server that accepts
/// the TCP connection but never responds doesn't stall launch indefinitely.
const RESUME_LIVE_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens the mirror/image-cache/event-bus for an already-authenticated
/// `client`, scoped to `(base_url, user_id)` (see module doc). Does
/// **not** touch the Keychain -- callers persist the `StoredSession`
/// themselves, since only they know whether this is a fresh login, resume,
/// or server switch (`root.rs` owns that bookkeeping).
///
/// Returns as soon as `Mirror::open` resolves; does not wait for the sync
/// engine's backfill to land -- `MainState`'s own `Mirror::changes()`
/// subscription (`Root::spawn_mirror_listener`) picks that up live.
async fn finish_bundle(
    client: JellyfinClient,
    base_url: &str,
    user_id: &str,
    offline: bool,
) -> Result<ConnectedBundle, String> {
    let (bus_rx, bus_handle) = jellyfin_core::EventBus::spawn(client.clone());
    let dir = app_support_dir_for(base_url, user_id);
    let mirror = Mirror::open(dir, client.clone(), bus_rx)
        .await
        .map_err(|e| e.to_string())?;
    let views = mirror.views();
    let image_cache = ImageCache::new(image_cache_dir_for(base_url, user_id), client.clone());
    Ok(ConnectedBundle {
        client,
        mirror,
        bus_handle,
        views,
        image_cache,
        offline,
    })
}

/// Fresh login: authenticate, then open the mirror. Returns the
/// `StoredSession` alongside the bundle so the caller can add it to the
/// session list and persist -- this doesn't touch the Keychain itself
/// (see `finish_bundle`).
pub(crate) async fn connect_flow(
    base_url: String,
    identity: ClientIdentity,
    username: String,
    password: String,
) -> Result<(ConnectedBundle, StoredSession), String> {
    let (client, auth) =
        JellyfinClient::authenticate_by_name(&base_url, identity, &username, &password)
            .await
            .map_err(|e| e.to_string())?;
    let token = auth
        .access_token
        .clone()
        .ok_or_else(|| "server did not return an access token".to_string())?;
    let user_id = client.user_id().map(|s| s.to_string()).unwrap_or_default();
    let display_name = auth
        .user
        .as_ref()
        .and_then(|u| u.name.clone())
        .or(Some(username));
    let bundle = finish_bundle(client, &base_url, &user_id, false).await?;
    let stored = StoredSession {
        base_url,
        token,
        user_id: Some(user_id),
        username: display_name,
        // No server version yet; `handle_connect_outcome` carries forward
        // any previously-known value before persisting, then refreshes it
        // for real via `/System/Info/Public`.
        server_version: None,
    };
    Ok((bundle, stored))
}

/// `POST /Users/AuthenticateWithQuickConnect` login, mirroring
/// `connect_flow`'s shape: called once `quick_connect_poll` reports
/// `authenticated: true` for `secret`, to exchange it for a real token.
pub(crate) async fn quick_connect_flow(
    base_url: String,
    identity: ClientIdentity,
    secret: String,
) -> Result<(ConnectedBundle, StoredSession), String> {
    let (client, auth) =
        JellyfinClient::authenticate_with_quick_connect(&base_url, identity, &secret)
            .await
            .map_err(|e| e.to_string())?;
    let token = auth
        .access_token
        .clone()
        .ok_or_else(|| "server did not return an access token".to_string())?;
    let user_id = client.user_id().map(|s| s.to_string()).unwrap_or_default();
    let display_name = auth.user.as_ref().and_then(|u| u.name.clone());
    let bundle = finish_bundle(client, &base_url, &user_id, false).await?;
    let stored = StoredSession {
        base_url,
        token,
        user_id: Some(user_id),
        username: display_name,
        // Same reasoning as `connect_flow`'s `stored` above.
        server_version: None,
    };
    Ok((bundle, stored))
}

/// Resume from a Keychain-stored session (`JellyfinClient::from_token` +
/// `with_user_id`). Sanity-checks the token with a cheap live request
/// before committing to it. Used both for app-launch resume and for
/// switching to an already-signed-in session.
///
/// docs/UX-SPEC.md §6 cache-first: if the live `/UserViews` check fails but
/// `app_support_dir_for`'s mirror database already exists on disk, this
/// proceeds anyway (`finish_bundle` only touches disk) and returns a
/// bundle marked `offline: true`. Only a resume with nothing cached yet
/// still falls back to an error.
///
/// When a populated mirror exists, the live check is never awaited at
/// all -- `finish_bundle` returns immediately, and the connectivity probe
/// runs in the background purely for diagnostics; `EventBus`'s own
/// `Connected`/`Disconnected` events are what actually drive `root.rs`'s
/// offline pill. `offline: false` here is therefore optimistic, not a
/// claim the server was reached.
pub(crate) async fn resume_flow(
    stored: StoredSession,
    identity: ClientIdentity,
) -> Result<ConnectedBundle, String> {
    let mut client = JellyfinClient::from_token(&stored.base_url, identity, &stored.token);
    if let Some(user_id) = &stored.user_id {
        client = client.with_user_id(user_id);
    }
    let user_id = stored.user_id.clone().unwrap_or_default();

    let mirror_dir = app_support_dir_for(&stored.base_url, &user_id);
    let mirror_populated = mirror_dir.join("mirror.db").is_file();

    if mirror_populated {
        tracing::info!(
            base_url = %stored.base_url,
            "resume: populated local mirror found -- painting Main from \
             cache immediately, checking connectivity in the background"
        );
        // Diagnostics-only: not awaited, doesn't gate `finish_bundle`
        // below (see doc comment above).
        let bg_client = client.clone();
        let bg_base_url = stored.base_url.clone();
        tokio::spawn(async move {
            let live_check =
                tokio::time::timeout(RESUME_LIVE_CHECK_TIMEOUT, bg_client.get_user_views()).await;
            match live_check {
                Ok(Ok(_)) => tracing::debug!(
                    base_url = %bg_base_url,
                    "resume: background connectivity check succeeded"
                ),
                Ok(Err(e)) => tracing::warn!(
                    base_url = %bg_base_url, error = %e,
                    "resume: background connectivity check failed -- \
                     EventBus's own connection attempt should surface this \
                     as the offline pill shortly"
                ),
                Err(_) => tracing::warn!(
                    base_url = %bg_base_url,
                    "resume: background connectivity check timed out after {}s",
                    RESUME_LIVE_CHECK_TIMEOUT.as_secs()
                ),
            }
        });
        return finish_bundle(client, &stored.base_url, &user_id, false).await;
    }

    // Nothing cached yet: unlike the populated-mirror path above, this
    // must wait for the live round trip.
    let live_check = tokio::time::timeout(RESUME_LIVE_CHECK_TIMEOUT, client.get_user_views()).await;
    let online = matches!(live_check, Ok(Ok(_)));
    if !online {
        return Err(match live_check {
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!(
                "server did not respond within {}s",
                RESUME_LIVE_CHECK_TIMEOUT.as_secs()
            ),
            Ok(Ok(_)) => unreachable!("`online` would be true"),
        });
    }

    finish_bundle(client, &stored.base_url, &user_id, false).await
}

// ---- Session lifecycle: connect / resume / Quick Connect / switch /
// teardown / add / remove ------------------------------------------------

impl Root {
    pub(crate) fn spawn_resume(&mut self, stored: StoredSession, cx: &mut Context<Self>) {
        self.seed_dns_for(&stored.base_url);
        if let Screen::Connect(state) = &mut self.screen {
            state.connecting = true;
            state.status = "Resuming saved session...".into();
        }
        self.connect_generation = self.connect_generation.wrapping_add(1);
        let generation = self.connect_generation;
        let identity = self.identity.clone();
        let resumed = stored.clone();
        self.bridge_or(
            cx,
            async move { crate::session::resume_flow(resumed, identity).await },
            || Err("resume task dropped".into()),
            move |root, outcome, cx| {
                // `resume_flow` returns no new `StoredSession` (token is
                // already on disk); pair the bundle with the one it took.
                let outcome = outcome.map(|bundle| (bundle, stored));
                root.handle_connect_outcome(generation, outcome, cx)
            },
        );
    }

    /// Connect button / Enter-in-password-field handler. No-op while
    /// Quick Connect is on (`render_connect` wires the button to
    /// `start_quick_connect` instead).
    pub(crate) fn on_connect_clicked(&mut self, cx: &mut Context<Self>) {
        let Screen::Connect(state) = &mut self.screen else {
            return;
        };
        if state.connecting || state.quick_connect {
            return;
        }
        let base_url = state.server.read(cx).content.trim().to_string();
        let username = state.username.read(cx).content.trim().to_string();
        let password = state.password.read(cx).content.clone();
        if base_url.is_empty() || username.is_empty() {
            state.status = "Server URL and username are required.".into();
            state.error_hint = None;
            cx.notify();
            return;
        }

        state.connecting = true;
        state.status = "Connecting...".into();
        state.error_hint = None;
        cx.notify();

        self.seed_dns_for(&base_url);
        self.connect_generation = self.connect_generation.wrapping_add(1);
        let generation = self.connect_generation;
        let identity = self.identity.clone();
        self.bridge_or(
            cx,
            async move {
                crate::session::connect_flow(base_url, identity, username, password).await
            },
            || Err("connect task dropped".into()),
            move |root, outcome, cx| root.handle_connect_outcome(generation, outcome, cx),
        );
    }

    /// Forces a fresh `Screen::Connect`, bumping `connect_generation` so a
    /// slow in-flight login/resume can't complete afterward and clobber
    /// whatever the caller starts next. Used by `JELLYBEAM_E2E` for
    /// deterministic Quick Connect testing.
    pub(crate) fn reset_to_connect_screen(&mut self, cx: &mut Context<Self>) {
        self.connect_generation = self.connect_generation.wrapping_add(1);
        self.teardown_main_state(cx);
        let root_weak = cx.entity().downgrade();
        self.screen = Screen::Connect(Box::new(new_connect_state(cx, root_weak)));
        cx.notify();
    }

    fn handle_connect_outcome(
        &mut self,
        generation: u64,
        outcome: Result<(ConnectedBundle, StoredSession), String>,
        cx: &mut Context<Self>,
    ) {
        if self.connect_generation != generation {
            // Superseded by a newer login/resume/reset attempt (see
            // `connect_generation`) -- discard regardless of outcome.
            tracing::info!("discarding stale connect/resume outcome (generation superseded)");
            return;
        }
        match outcome {
            Ok((bundle, mut stored)) => {
                tracing::info!(views = bundle.views.len(), base_url = %stored.base_url, "connected; showing library");
                // Fresh logins mint `stored` with no `server_version` yet;
                // carry forward whatever was last confirmed before
                // persisting (no-op for a resume, which already has it).
                self.preserve_known_server_version(&mut stored);
                self.seed_server_version(&stored);
                self.sessions.add_or_update(stored.clone());
                if let Err(e) = crate::keychain::save_sessions(&self.sessions) {
                    tracing::warn!(error = %e, "failed to persist session list to Keychain");
                }
                let main_state = main_state_from_bundle(
                    bundle,
                    stored.base_url,
                    self.runtime.clone(),
                    &self.app_settings,
                    cx,
                );
                self.screen = Screen::Main(Box::new(main_state));
                self.spawn_mirror_listener(cx);
                self.spawn_bus_listener(cx);
                self.spawn_sync_activity_listener(cx);
                self.spawn_pending_stop_replay();
                // Refresh the server version for real now that `Screen::Main`
                // is live; seeded from the stored value above so
                // `server_at_least` isn't unknown meanwhile.
                self.spawn_server_version_refresh(cx);
                // Land on the configured startup screen once, right after
                // login/resume/Quick Connect completes -- deliberately not
                // applied from `handle_switch_outcome`'s server-switch path
                // below, where landing on Home (`Nav::new()`'s default) is
                // the least surprising outcome for a deliberate mid-session
                // switch.
                self.apply_startup_screen(cx);
                // The connect/resume above just resolved this server's
                // host for real -- bank the answer for next launch (see
                // `persist_dns_seed`).
                self.persist_dns_seed();
                // "exec -> Main painted" warm-start timing (see
                // `warm_start_at`); only set on the launch-time
                // auto-resume path, never on a fresh/manual login.
                if let Some(launched_at) = self.warm_start_at.take() {
                    tracing::info!(
                        elapsed_ms = launched_at.elapsed().as_millis() as u64,
                        "JELLYBEAM_WARM_START: exec -> Main painted"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "connect/resume failed");
                let hint = transport_error_hint_for_message(&e);
                if let Screen::Connect(state) = &mut self.screen {
                    state.connecting = false;
                    state.status = connect_error_line(&e).into();
                    state.error_hint = hint;
                } else {
                    // Not expected to occur; fall back to a fresh
                    // Connect screen rather than getting stuck.
                    let root_weak = cx.entity().downgrade();
                    let mut state = new_connect_state(cx, root_weak);
                    state.status = connect_error_line(&e).into();
                    state.error_hint = hint;
                    self.screen = Screen::Connect(Box::new(state));
                }
            }
        }
        cx.notify();
    }

    // --- Quick Connect ----------------------------------------------------

    /// "Use Quick Connect" toggle. Flips the Connect form into the
    /// code-flow UI and, when turning on, immediately kicks off Initiate --
    /// no separate "Start" button.
    pub(crate) fn toggle_quick_connect(&mut self, cx: &mut Context<Self>) {
        // Also invalidates any in-flight resume/connect (`connect_generation`):
        // choosing Quick Connect must supersede a slow earlier attempt.
        self.connect_generation = self.connect_generation.wrapping_add(1);
        let Screen::Connect(state) = &mut self.screen else {
            return;
        };
        state.quick_connect = !state.quick_connect;
        state.qc_code = None;
        state.qc_secret = None;
        state.status = SharedString::default();
        state.error_hint = None;
        state.connecting = false;
        state.qc_generation = state.qc_generation.wrapping_add(1);
        let turning_on = state.quick_connect;
        cx.notify();
        if turning_on {
            self.start_quick_connect(cx);
        }
    }

    pub(crate) fn start_quick_connect(&mut self, cx: &mut Context<Self>) {
        let Screen::Connect(state) = &mut self.screen else {
            return;
        };
        let base_url = state.server.read(cx).content.trim().to_string();
        if base_url.is_empty() {
            state.status = "Server URL is required.".into();
            state.error_hint = None;
            cx.notify();
            return;
        }
        state.qc_generation = state.qc_generation.wrapping_add(1);
        let generation = state.qc_generation;
        state.status = "Requesting a Quick Connect code...".into();
        state.error_hint = None;
        cx.notify();

        self.seed_dns_for(&base_url);
        let identity = self.identity.clone();
        let fetch_url = base_url.clone();
        self.bridge_or(
            cx,
            async move { JellyfinClient::quick_connect_initiate(&fetch_url, &identity).await },
            || {
                Err(jellyfin_api::ApiError::Transport(
                    "quick connect initiate task dropped".to_string(),
                ))
            },
            move |root, result, cx| root.handle_qc_initiated(generation, base_url, result, cx),
        );
    }

    fn handle_qc_initiated(
        &mut self,
        generation: u64,
        base_url: String,
        result: Result<jellyfin_api::models::QuickConnectResult, jellyfin_api::ApiError>,
        cx: &mut Context<Self>,
    ) {
        let Screen::Connect(state) = &mut self.screen else {
            return;
        };
        if state.qc_generation != generation {
            return; // superseded by a newer toggle/retry -- see qc_generation's doc comment.
        }
        match result {
            Ok(qc) => {
                let (Some(code), Some(secret)) = (qc.code, qc.secret) else {
                    state.status = "Server returned an incomplete Quick Connect response.".into();
                    state.error_hint = None;
                    cx.notify();
                    return;
                };
                state.qc_code = Some(code);
                state.qc_secret = Some(secret.clone());
                state.status = "Enter this code on another signed-in device.".into();
                state.error_hint = None;
                cx.notify();
                self.spawn_qc_poll(generation, base_url, secret, cx);
            }
            Err(e) => {
                state.error_hint = transport_error_hint_for_api_error(&e);
                state.status = format!("Quick Connect unavailable: {e}").into();
                cx.notify();
            }
        }
    }

    /// Polls `GET /QuickConnect/Connect` every 2s until `authenticated:
    /// true`, an error, or `generation` is superseded (see `qc_generation`
    /// on `ConnectState`).
    fn spawn_qc_poll(&self, generation: u64, base_url: String, secret: String, cx: &Context<Self>) {
        let identity = self.identity.clone();
        let runtime = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            loop {
                // GPUI's background-executor timer, not
                // `tokio::time::sleep` -- this closure runs on `cx`'s
                // executor, which has no Tokio reactor (same shape as
                // `spawn_osd_tick`).
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let still_current = this
                    .update(cx, |root, _cx| {
                        matches!(&root.screen, Screen::Connect(s) if s.qc_generation == generation)
                    })
                    .unwrap_or(false);
                if !still_current {
                    return;
                }

                let (tx, rx) = tokio::sync::oneshot::channel();
                let poll_url = base_url.clone();
                let poll_secret = secret.clone();
                let poll_identity = identity.clone();
                runtime.spawn(async move {
                    let result =
                        JellyfinClient::quick_connect_poll(&poll_url, &poll_identity, &poll_secret)
                            .await;
                    let _ = tx.send(result);
                });
                let Ok(result) = rx.await else { return };
                match result {
                    Ok(qc) if qc.authenticated == Some(true) => {
                        let base_url = base_url.clone();
                        let secret = secret.clone();
                        this.update(cx, |root, cx| {
                            root.complete_quick_connect(generation, base_url, secret, cx)
                        })
                        .ok();
                        return;
                    }
                    Ok(_) => continue, // not yet approved -- poll again in 2s.
                    Err(e) => {
                        this.update(cx, |root, cx| root.handle_qc_poll_error(generation, e, cx))
                            .ok();
                        return;
                    }
                }
            }
        })
        .detach();
    }

    fn handle_qc_poll_error(
        &mut self,
        generation: u64,
        err: jellyfin_api::ApiError,
        cx: &mut Context<Self>,
    ) {
        let Screen::Connect(state) = &mut self.screen else {
            return;
        };
        if state.qc_generation != generation {
            return;
        }
        state.error_hint = transport_error_hint_for_api_error(&err);
        state.status = format!("Quick Connect failed: {err}").into();
        cx.notify();
    }

    fn complete_quick_connect(
        &mut self,
        generation: u64,
        base_url: String,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        {
            let Screen::Connect(state) = &mut self.screen else {
                return;
            };
            if state.qc_generation != generation {
                return;
            }
            state.status = "Approved -- signing in...".into();
            state.error_hint = None;
            cx.notify();
        }
        self.connect_generation = self.connect_generation.wrapping_add(1);
        let connect_generation = self.connect_generation;
        let identity = self.identity.clone();
        self.bridge_or(
            cx,
            async move { crate::session::quick_connect_flow(base_url, identity, secret).await },
            || Err("quick connect task dropped".into()),
            move |root, outcome, cx| root.handle_connect_outcome(connect_generation, outcome, cx),
        );
    }

    /// Subscribes once to `Mirror::changes()` for the app's whole
    /// Main-screen life (docs/DATA.md §2; docs/UX-SPEC.md §5). Uses the lag-safe
    /// `recv_changes` helper; any change re-queries wholesale rather than
    /// applying surgical deltas.
    fn spawn_mirror_listener(&self, cx: &Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let mut rx = state.mirror.changes();
        // `media_cache::recv_changes` uses `tokio::time::sleep_until`
        // internally (its debounce window), which needs a Tokio time
        // driver entered on the polling thread -- GPUI's `cx.spawn`
        // executor has none. Run the loop on `self.runtime` instead and
        // forward each change over a plain channel, which `cx.spawn` can
        // drain without any Tokio driver (same shape as
        // `spawn_player_events_task`).
        let runtime = self.runtime.clone();
        let (tx, mut change_rx) = tokio::sync::mpsc::unbounded_channel();
        runtime.spawn(async move {
            while let Some(change) = media_cache::recv_changes(&mut rx).await {
                if tx.send(change).is_err() {
                    break;
                }
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(change) = change_rx.recv().await {
                if this
                    .update(cx, |root, cx| {
                        crate::panic_log::catch_and_log(
                            "mirror-change handler",
                            std::panic::AssertUnwindSafe(|| root.on_mirror_change(change, cx)),
                        );
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Bridges `Mirror::sync_activity`'s watch channel into `cx.notify` so
    /// the sidebar sync pill repaints as syncs start/progress/end.
    /// `watch::Receiver::changed` needs no Tokio driver, so polling it
    /// directly on GPUI's executor is safe (unlike `recv_changes` above).
    /// Replays a previous launch's quit-time pending stop report (see
    /// `pending_report`'s module docs) once a session is established.
    /// Detached: nothing in the UI depends on its outcome.
    fn spawn_pending_stop_replay(&self) {
        let Some(state) = self.main_state() else {
            return;
        };
        let base_url = state.client.base_url().to_string();
        let Some(report) = crate::pending_report::load_for_replay(&base_url) else {
            return;
        };
        let client = state.client.clone();
        self.runtime
            .spawn(crate::pending_report::replay(client, report));
    }

    fn spawn_sync_activity_listener(&self, cx: &Context<Self>) {
        let Some(state) = self.main_state() else {
            return;
        };
        let mut rx = state.mirror.sync_activity();
        cx.spawn(async move |this, cx| {
            while rx.changed().await.is_ok() {
                let settled = matches!(*rx.borrow(), media_cache::SyncActivity::Idle);
                if this
                    .update(cx, |root, cx| {
                        // Mirror changes are ignored while a breadth sync is
                        // active (see `on_mirror_change`); refresh once a
                        // terminal Idle lands instead. `initial_sync`
                        // re-emits a final Idle after its flag clears so
                        // this is never coalesced away.
                        if settled {
                            root.refresh_active_library(cx);
                            // Rescan is cheap: everything already on disk
                            // is skipped, cheaper than tracking touched rows.
                            root.request_poster_warm();
                            // Same settle signal drives the once-per-session
                            // hero preload (see `maybe_auto_preload_hero`).
                            root.maybe_auto_preload_hero(cx);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Asks the background poster warmer for a (re)scan, prioritizing
    /// whichever library is open. Reads `state.library`, not `nav.current`:
    /// it stays set while on a Detail page reached from that library.
    fn request_poster_warm(&self) {
        let Some(state) = self.main_state() else {
            return;
        };
        state
            .image_warm
            .request_pass(state.library.as_ref().map(|lib| lib.view_id.clone()));
    }

    /// The Home hero's Resume item gets at most one automatic dark preload
    /// per session, fired from the first *settled* post-connect sync
    /// (`spawn_sync_activity_listener`'s terminal `Idle`), not paint time --
    /// on a fresh/reconnected mirror the paint-time hero is often not the
    /// final Continue Watching item, and preloading it wastes bandwidth
    /// once a later sync replaces it.
    ///
    /// Once attempted (`MainState::hero_preload_attempted`), a later
    /// hero-candidate change must never retarget this on its own -- only an
    /// explicit user signal can (`PreloadTrigger::Hover`, throttled, or
    /// `PreloadTrigger::Deliberate` from opening/focusing a Detail page).
    fn maybe_auto_preload_hero(&mut self, cx: &mut Context<Self>) {
        let Screen::Main(state) = &mut self.screen else {
            return;
        };
        if state.hero_preload_attempted {
            return;
        }
        // Don't burn the one-shot attempt while the setting is off -- a
        // mid-session re-enable still gets a shot at a later settle.
        if !self.app_settings.preload {
            return;
        }
        state.hero_preload_attempted = true;
        // A deliberate/hover preload already in flight or ready must not
        // be discarded in favor of this speculative guess.
        if state.preload_target.is_some() {
            return;
        }
        // No hero to preload; the attempt is still spent -- a hero
        // appearing later must not retarget this automatically.
        let Some(item_id) = crate::home::hero_candidate(&state.home).map(|c| c.id.clone()) else {
            return;
        };
        self.preload_item(item_id, PreloadTrigger::Deliberate, cx);
    }

    fn on_mirror_change(&mut self, _change: media_cache::MirrorChange, cx: &mut Context<Self>) {
        // Read before borrowing `self.screen` below so the borrow checker
        // sees two disjoint field reads, not an interleaved one.
        let hidden_libraries = self.app_settings.hidden_home_libraries();
        let hide_watched_latest = self.app_settings.hide_watched_latest;
        let Some(state) = self.main_state_mut() else {
            return;
        };
        let mirror = state.mirror.clone();
        // Re-fetched on every change (not just `ViewsChanged`) since Main
        // paints immediately from what may be an empty mirror -- a session
        // starting from a genuinely empty mirror needs this backfill
        // signal to ever populate the sidebar (ARCHITECTURE.md's "Known
        // simplifications" coarse-refresh shape).
        state.views = mirror.views();
        // Order-freeze Home shelves during the mirror's initial sync or a
        // schema-rebuild resync (see `HomeState::refresh`).
        state.home.refresh(
            &mirror,
            &state.views,
            &hidden_libraries,
            hide_watched_latest,
            mirror.is_syncing(),
        );
        // A breadth sync can commit several pages a second; skip
        // re-parsing the library on every page and refresh once from the
        // settled snapshot on terminal Idle instead (`is_syncing` also
        // covers the reconcile mismatch sweep). Rebuild is spawned after
        // this borrow ends and runs off-thread.
        let refresh_library = !mirror.is_syncing();
        if let Some(detail) = &mut state.detail {
            if let Some(fresh) = mirror.item(&detail.item_id) {
                // MERGE, don't replace: the mirror blob never carries
                // `MediaStreams`/`Chapters`/`Trickplay`/`People`/
                // `Overview`/`Genres` (docs/DATA.md's bulk-sync field list), so
                // a wholesale swap would wipe enrichment already grafted
                // on by `apply_detail_enrichment`.
                match &mut detail.dto {
                    Some(base) => crate::detail::merge_enrichment(base, fresh),
                    None => detail.dto = Some(fresh),
                }
            }
            // Seasons/episodes are plain mirror reads, so a live
            // `MirrorChange` can just re-run them here -- same coarse
            // "any change re-runs the query" shape used for home/library.
            if detail.is_series {
                detail.seasons = mirror.children(&detail.item_id, Sort::IndexNumber, 0, 200);
                if let Some(season) = detail.seasons.get(detail.selected_season).cloned() {
                    detail.episodes = mirror.children(&season.id, Sort::IndexNumber, 0, 500);
                } else {
                    detail.episodes.clear();
                }
                detail.episode_focus.clamp(detail.episodes.len());
                detail.next_episode =
                    crate::detail::find_series_next_episode(&mirror, &detail.seasons);
            }
        }
        if state.search.open {
            state.search.run_query(&mirror);
        }
        if refresh_library {
            self.spawn_library_rebuild(cx);
        }
        cx.notify();
    }
    // --- Multi-server/user -------------------------------------------------

    /// Tears down the current `Main` screen, if any: reports final
    /// playback progress (`stop_playback`), then explicitly shuts down the
    /// old `EventBusHandle` (rather than relying on `Drop`'s best-effort
    /// abort) before the rest of `MainState` drops. Leaves `self.screen` as
    /// `Screen::Switching`; callers set the real next screen.
    fn teardown_main_state(&mut self, cx: &mut Context<Self>) {
        self.stop_playback(cx);
        // `stop_playback` early-returns in Browse -- exactly where an idle
        // preload lives. The paused stream and in-flight task must not
        // survive into the next server's session.
        if let Screen::Main(state) = &mut self.screen {
            state.playback_generation.fetch_add(1, Ordering::Relaxed);
            // Holds a `Mirror`/`ImageCache` clone for the server being torn
            // down -- must die with the session, same as `preload_task`
            // below (dropping the handle would only detach it).
            state.image_warm.abort();
            if let Some(handle) = state.preload_task.take() {
                handle.abort();
            }
            state.preload_target = None;
            let had_preload = state.preload.is_some();
            crate::root_playback::discard_preload(state, self.video.player(), "server teardown");
            if had_preload {
                if let Err(e) = self.video.player().stop() {
                    tracing::warn!(error = %e, "stopping idle preload during teardown failed");
                }
            }
        }
        let previous = std::mem::replace(&mut self.screen, Screen::Switching);
        match previous {
            Screen::Main(boxed) => {
                let MainState { _bus_handle, .. } = *boxed;
                self.runtime.spawn(async move {
                    _bus_handle.shutdown().await;
                });
            }
            other => self.screen = other, // wasn't Main -- nothing to tear down.
        }
    }

    /// "Add server" (Settings sheet): tears down the current `MainState`
    /// and shows a fresh Connect form, keeping `self.sessions` intact so
    /// the new login is *added* rather than replacing it.
    pub(crate) fn start_add_server(&mut self, cx: &mut Context<Self>) {
        self.teardown_main_state(cx);
        let root_weak = cx.entity().downgrade();
        let mut state = new_connect_state(cx, root_weak);
        // A signed-in session to fall back to gets this form a way out
        // (Cancel/Escape -- see `cancel_add_server`).
        state.can_cancel = !self.sessions.sessions.is_empty();
        self.screen = Screen::Connect(Box::new(state));
        cx.notify();
    }

    /// Backs out of an "Add server" Connect form. `start_add_server`
    /// already tore the old `MainState` down, so "back" means resuming the
    /// still-active stored session (`Root::new`'s own launch path), which
    /// also invalidates any in-flight Quick Connect poll from the
    /// abandoned form.
    pub(crate) fn cancel_add_server(&mut self, cx: &mut Context<Self>) {
        let can_cancel = matches!(
            &self.screen,
            Screen::Connect(s) if s.can_cancel && !s.connecting
        );
        if !can_cancel {
            return;
        }
        let Some(stored) = self.sessions.active_session().cloned() else {
            return;
        };
        let root_weak = cx.entity().downgrade();
        self.screen = Screen::Connect(Box::new(new_connect_state_connecting(cx, root_weak)));
        self.spawn_resume(stored, cx);
        cx.notify();
    }

    /// Switches to an already-signed-in session at `ix` (Settings sheet's
    /// "Switch" button). Unlike `start_add_server`, never shows the
    /// Connect form -- goes straight through `resume_flow`, since the
    /// target session's token is already known.
    pub(crate) fn switch_to_session(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix == self.sessions.active || ix >= self.sessions.sessions.len() {
            return;
        }
        let target = self.sessions.sessions[ix].clone();
        self.sessions.active = ix;
        if let Err(e) = crate::keychain::save_sessions(&self.sessions) {
            tracing::warn!(error = %e, "failed to persist session list to Keychain");
        }
        self.teardown_main_state(cx);
        cx.notify(); // paints the `Screen::Switching` loading state.

        let identity = self.identity.clone();
        self.bridge_or(
            cx,
            async move { crate::session::resume_flow(target, identity).await },
            || Err("switch task dropped".into()),
            move |root, outcome, cx| root.handle_switch_outcome(outcome, cx),
        );
    }

    fn handle_switch_outcome(
        &mut self,
        outcome: Result<ConnectedBundle, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(bundle) => {
                let base_url = self
                    .sessions
                    .active_session()
                    .map(|s| s.base_url.clone())
                    .unwrap_or_default();
                // `switch_to_session` already made the target active; seed
                // from its last-persisted `server_version` while the live
                // refresh below is in flight.
                if let Some(active) = self.sessions.active_session().cloned() {
                    self.seed_server_version(&active);
                }
                let main_state = main_state_from_bundle(
                    bundle,
                    base_url,
                    self.runtime.clone(),
                    &self.app_settings,
                    cx,
                );
                self.screen = Screen::Main(Box::new(main_state));
                self.spawn_mirror_listener(cx);
                self.spawn_bus_listener(cx);
                self.spawn_sync_activity_listener(cx);
                self.spawn_pending_stop_replay();
                self.spawn_server_version_refresh(cx);
            }
            Err(e) => {
                tracing::warn!(error = %e, "server switch failed");
                let root_weak = cx.entity().downgrade();
                let mut state = new_connect_state(cx, root_weak);
                state.status = format!("Switch failed: {e}").into();
                self.screen = Screen::Connect(Box::new(state));
            }
        }
        cx.notify();
    }

    /// Removes a *non-active* session (Settings sheet's "Remove" button,
    /// only shown for non-active rows -- removing the active session would
    /// need a teardown this method doesn't do; switch away first).
    pub(crate) fn remove_session(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix == self.sessions.active {
            return;
        }
        self.sessions.remove(ix);
        if let Err(e) = crate::keychain::save_sessions(&self.sessions) {
            tracing::warn!(error = %e, "failed to persist session list to Keychain");
        }
        cx.notify();
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Pins `server_scope_dir` stability: same `(base_url, user_id)` must
    /// always map to the same directory.
    #[test]
    fn app_support_dir_for_is_stable_across_repeated_calls() {
        // Reads HOME (via app_support_dir_for) -- must serialize with
        // other HOME-mutating tests to avoid racing their temp-HOME swaps.
        let _guard = crate::test_support::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let base_url = "http://example.com:8096";
        let user_id = "user-abc-123";
        let first = app_support_dir_for(base_url, user_id);
        for _ in 0..20 {
            assert_eq!(
                app_support_dir_for(base_url, user_id),
                first,
                "app_support_dir_for must be a stable function of its inputs, \
                 not vary from call to call"
            );
        }
        // Confirm it's a function of the inputs, not a constant.
        assert_ne!(
            app_support_dir_for("http://other.example.com:8096", user_id),
            first
        );
        assert_ne!(app_support_dir_for(base_url, "different-user"), first);
    }

    /// Pins docs/UX-SPEC.md §6 cache-first resume: `resume_flow` must succeed
    /// (not bounce to Connect) and return without waiting on the live
    /// check when a populated mirror exists, even against an unreachable
    /// server (`127.0.0.1:1`, connection refused immediately).
    #[test]
    fn resume_flow_paints_from_cache_without_awaiting_the_live_check() {
        crate::test_support::with_temp_home("session-resume-offline", || {
            let rt = tokio::runtime::Runtime::new().expect("build a tokio runtime for this test");
            rt.block_on(async {
                // Nothing listens on port 1 -- connection refused
                // immediately, so "unreachable" here is unambiguous.
                let base_url = "http://127.0.0.1:1".to_string();
                let user_id = "offline-test-user".to_string();

                // Simulate a previously-synced mirror: `finish_bundle`/
                // `Mirror::open` would have created this dir + `mirror.db`.
                let dir = app_support_dir_for(&base_url, &user_id);
                std::fs::create_dir_all(&dir).expect("create the mirror dir");
                std::fs::write(dir.join("mirror.db"), []).expect("touch mirror.db");

                let stored = StoredSession {
                    base_url: base_url.clone(),
                    token: "fake-token".to_string(),
                    user_id: Some(user_id),
                    username: Some("Offline Tester".to_string()),
                    server_version: None,
                };
                let identity = ClientIdentity {
                    client: "Jellybeam-Test".to_string(),
                    device: "test".to_string(),
                    device_id: "jellybeam-test-device".to_string(),
                    version: "0.0.0".to_string(),
                };

                let start = std::time::Instant::now();
                let bundle = resume_flow(stored, identity).await.expect(
                    "resume_flow must succeed from cache when the mirror is \
                         already populated, instead of bouncing to Connect",
                );
                let elapsed = start.elapsed();
                assert!(
                    !bundle.offline,
                    "resume_flow must return optimistically \
                     online (offline: false) for a populated-mirror resume \
                     -- EventBus's own independent Connected/Disconnected \
                     detection is what corrects this afterward, not this \
                     function's return value"
                );
                assert!(
                    elapsed < RESUME_LIVE_CHECK_TIMEOUT,
                    "resume_flow must not block on the live connectivity \
                     check for a populated mirror -- took {elapsed:?}, \
                     which is suspiciously close to/over the {}s timeout \
                     it should no longer be waiting on",
                    RESUME_LIVE_CHECK_TIMEOUT.as_secs()
                );

                bundle.bus_handle.shutdown().await;
            });
        });
    }

    /// Pins the complementary case: with nothing cached, `resume_flow`
    /// must still surface a real error rather than succeed offline empty.
    #[test]
    fn resume_flow_still_errors_when_unreachable_and_nothing_is_cached() {
        crate::test_support::with_temp_home("session-resume-offline-uncached", || {
            let rt = tokio::runtime::Runtime::new().expect("build a tokio runtime for this test");
            rt.block_on(async {
                let base_url = "http://127.0.0.1:1".to_string();
                let stored = StoredSession {
                    base_url,
                    token: "fake-token".to_string(),
                    user_id: Some("never-synced-user".to_string()),
                    username: None,
                    server_version: None,
                };
                let identity = ClientIdentity {
                    client: "Jellybeam-Test".to_string(),
                    device: "test".to_string(),
                    device_id: "jellybeam-test-device".to_string(),
                    version: "0.0.0".to_string(),
                };

                let result = resume_flow(stored, identity).await;
                assert!(
                    result.is_err(),
                    "resume_flow must still error when unreachable and there's \
                     no cached mirror to fall back to"
                );
            });
        });
    }
}
