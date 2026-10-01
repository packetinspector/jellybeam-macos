//! WebSocket client: connects to `/socket`, decodes the handful of server
//! events Jellybeam cares about into [`crate::ServerEvent`], and responds to
//! `ForceKeepAlive` so the server doesn't drop the connection.
//!
//! The access token travels in the handshake's `Authorization` header
//! (`MediaBrowser ... Token="..."`, the same value the REST client sends),
//! not as a `?api_key=` query param -- Jellyfin 12 no longer accepts that
//! query form by default (legacy authorization, removed in 12.0). Only
//! `deviceId` stays in the URL.
//!
//! One connection per call to [`crate::JellyfinClient::connect_ws`], no
//! auto-reconnect — that's jellyfin-core's job (it owns the session and can
//! observe the returned channel closing to trigger a reconnect).

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::models;
use crate::util::{describe_error_chain, percent_encode};
use crate::{ApiError, ClientIdentity, ServerEvent};

const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Timeout for establishing the WebSocket connection: without one, a
/// stalled TCP/TLS/HTTP-upgrade handshake against an unreachable or
/// misbehaving server would hang `connect_ws` forever. Once the
/// socket is up, it's expected to be long-lived, so this only bounds the
/// initial handshake, not the connection's lifetime.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Rewrite an `http(s)://` base URL into the matching `ws(s)://` form.
fn ws_base(base_url: &str) -> String {
    if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // Already scheme-less or an unrecognized scheme; assume plaintext.
        format!("ws://{base_url}")
    }
}

pub(crate) async fn connect(
    base_url: &str,
    identity: &ClientIdentity,
    token: &str,
) -> Result<mpsc::Receiver<ServerEvent>, ApiError> {
    let url = format!(
        "{}/socket?deviceId={}",
        ws_base(base_url),
        percent_encode(&identity.device_id),
    );
    let uri: tokio_tungstenite::tungstenite::http::Uri =
        url.parse()
            .map_err(|e: tokio_tungstenite::tungstenite::http::uri::InvalidUri| {
                ApiError::Transport(describe_error_chain(&e))
            })?;
    let request = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
        .with_header("Authorization", crate::auth_header(identity, Some(token)));

    let connect_result = tokio::time::timeout(
        WS_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_elapsed| {
        ApiError::Transport(format!(
            "websocket connect timed out after {WS_CONNECT_TIMEOUT:?}"
        ))
    })?;

    let (stream, _response) = connect_result.map_err(map_connect_error)?;

    let (mut write, mut read) = stream.split();
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

    tokio::spawn(async move {
        loop {
            // Race the next server frame against the receiver being
            // dropped, rather than only checking after `tx.send()`: with
            // only the `while let Some(next) =
            // read.next().await` loop, a caller that drops its receiver
            // isn't noticed until the *next* server message arrives, which
            // on an idle connection can be indefinitely -- the task and
            // its socket leak until the server happens to speak again.
            tokio::select! {
                biased;
                _ = tx.closed() => {
                    tracing::debug!("jellyfin websocket receiver dropped; closing");
                    let _ = write.send(Message::Close(None)).await;
                    break;
                }
                next = read.next() => {
                    let Some(next) = next else { break };
                    let msg = match next {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::debug!(error = %e, "jellyfin websocket read error; closing");
                            break;
                        }
                    };
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => break,
                        Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
                            continue
                        }
                    };

                    let Some(event) = decode_message(&text) else {
                        continue;
                    };

                    // A ForceKeepAlive from the server means "send KeepAlive or I'll
                    // drop you"; reply immediately, in addition to surfacing the
                    // event to the caller.
                    if matches!(event, ServerEvent::ForceKeepAlive) {
                        let ack = Message::Text(r#"{"MessageType":"KeepAlive"}"#.to_string());
                        if let Err(e) = write.send(ack).await {
                            tracing::debug!(error = %e, "failed to send KeepAlive; closing");
                            break;
                        }
                    }

                    if tx.send(event).await.is_err() {
                        // Receiver dropped between the read completing and
                        // here (the `tx.closed()` race above only catches
                        // the common case of an idle connection); still
                        // close cleanly rather than leaving the socket
                        // half-open.
                        let _ = write.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        }
    });

    Ok(rx)
}

/// Map a failed WebSocket handshake to an [`ApiError`]. A `401` HTTP
/// response during the upgrade means the auth token was
/// rejected -- surfaced as [`ApiError::Unauthorized`], matching how the
/// REST client (`check_status` in `lib.rs`) treats the same status, so
/// callers can handle "token needs refreshing" uniformly regardless of
/// which client method they called.
fn map_connect_error(e: tokio_tungstenite::tungstenite::Error) -> ApiError {
    if let tokio_tungstenite::tungstenite::Error::Http(response) = &e {
        if response.status() == tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED {
            return ApiError::Unauthorized;
        }
    }
    ApiError::Transport(describe_error_chain(&e))
}

/// Decode one raw WebSocket text frame into a [`ServerEvent`].
///
/// Returns `None` only when the frame isn't even valid JSON (can't happen
/// against a well-behaved server, but malformed input must never panic).
/// Any recognized `MessageType` whose `Data` fails to decode against our
/// pinned model still yields `Some` (falls back to `Ignored`) rather than
/// silently dropping the frame.
// `pub` inside a private module: invisible to normal downstream users of
// this crate; the only external path to it is the `cfg(fuzzing)`-gated
// re-export in lib.rs, used by the fuzz workspace to exercise this decoder
// directly.
pub fn decode_message(text: &str) -> Option<ServerEvent> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let message_type = value.get("MessageType").and_then(|v| v.as_str())?;

    let event = match message_type {
        "LibraryChanged" => {
            let data = value
                .get("Data")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match serde_json::from_value::<models::LibraryUpdateInfo>(data) {
                Ok(info) => ServerEvent::LibraryChanged {
                    added: info.items_added,
                    updated: info.items_updated,
                    removed: info.items_removed,
                },
                Err(e) => {
                    tracing::debug!(error = %e, "failed to decode LibraryUpdateInfo");
                    ServerEvent::Ignored(message_type.to_string())
                }
            }
        }
        "UserDataChanged" => {
            let data = value
                .get("Data")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match serde_json::from_value::<models::UserDataChangeInfo>(data) {
                Ok(info) => {
                    let item_userdata = info
                        .user_data_list
                        .into_iter()
                        .filter_map(|dto| dto.item_id.map(|id| (id.to_string(), dto)))
                        .collect();
                    ServerEvent::UserDataChanged { item_userdata }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "failed to decode UserDataChangeInfo");
                    ServerEvent::Ignored(message_type.to_string())
                }
            }
        }
        "ForceKeepAlive" => ServerEvent::ForceKeepAlive,
        other => ServerEvent::Ignored(other.to_string()),
    };

    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N1 regression test: a 401 during the WebSocket upgrade handshake
    /// (rejected/expired token) must map to `ApiError::Unauthorized`, not a
    /// generic `Transport` error, so callers can treat it the same way as a
    /// 401 from the REST client (see `check_status` in `lib.rs`).
    #[test]
    fn map_connect_error_translates_401_to_unauthorized() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(401)
            .body(None)
            .expect("builds a response with no body");
        let err = tokio_tungstenite::tungstenite::Error::Http(response);
        assert!(matches!(map_connect_error(err), ApiError::Unauthorized));
    }

    #[test]
    fn map_connect_error_translates_other_http_status_to_transport() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(500)
            .body(None)
            .expect("builds a response with no body");
        let err = tokio_tungstenite::tungstenite::Error::Http(response);
        assert!(matches!(map_connect_error(err), ApiError::Transport(_)));
    }

    #[test]
    fn map_connect_error_translates_non_http_error_to_transport() {
        let err = tokio_tungstenite::tungstenite::Error::AlreadyClosed;
        assert!(matches!(map_connect_error(err), ApiError::Transport(_)));
    }

    #[test]
    fn decodes_library_changed() {
        let raw = r#"{"MessageType":"LibraryChanged","Data":{"ItemsAdded":["a","b"],"ItemsUpdated":["c"],"ItemsRemoved":[]}}"#;
        match decode_message(raw) {
            Some(ServerEvent::LibraryChanged {
                added,
                updated,
                removed,
            }) => {
                assert_eq!(added, vec!["a".to_string(), "b".to_string()]);
                assert_eq!(updated, vec!["c".to_string()]);
                assert!(removed.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decodes_user_data_changed() {
        let raw = r#"{"MessageType":"UserDataChanged","Data":{"UserDataList":[{"Key":"k","ItemId":"e2f5a5f1-1a0b-4b3a-9c2e-000000000001","Played":true,"PlaybackPositionTicks":123}]}}"#;
        match decode_message(raw) {
            Some(ServerEvent::UserDataChanged { item_userdata }) => {
                assert_eq!(item_userdata.len(), 1);
                let (id, dto) = &item_userdata[0];
                assert_eq!(id, "e2f5a5f1-1a0b-4b3a-9c2e-000000000001");
                assert_eq!(dto.played, Some(true));
                assert_eq!(dto.playback_position_ticks, Some(123));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decodes_force_keep_alive() {
        let raw = r#"{"MessageType":"ForceKeepAlive","Data":30}"#;
        assert!(matches!(
            decode_message(raw),
            Some(ServerEvent::ForceKeepAlive)
        ));
    }

    #[test]
    fn unknown_message_types_are_ignored_by_name() {
        let raw = r#"{"MessageType":"SomeFutureMessageType","Data":{}}"#;
        match decode_message(raw) {
            Some(ServerEvent::Ignored(name)) => assert_eq!(name, "SomeFutureMessageType"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn malformed_json_does_not_panic() {
        assert!(decode_message("not json").is_none());
        assert!(decode_message("").is_none());
        assert!(decode_message("{}").is_none());
    }

    #[test]
    fn library_changed_with_wrong_shaped_data_falls_back_to_ignored() {
        // Data present but not an object at all -- must not panic, and must
        // still surface something useful rather than silently dropping it.
        let raw = r#"{"MessageType":"LibraryChanged","Data":"unexpected-string"}"#;
        match decode_message(raw) {
            Some(ServerEvent::Ignored(name)) => assert_eq!(name, "LibraryChanged"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn ws_base_rewrites_scheme() {
        assert_eq!(ws_base("http://localhost:8096"), "ws://localhost:8096");
        assert_eq!(
            ws_base("https://demo.jellyfin.org/stable"),
            "wss://demo.jellyfin.org/stable"
        );
    }

    /// M3 regression test, against a real local WebSocket server (not
    /// `localhost:8096`): before the fix, the read task only checked
    /// whether the receiver had been dropped *after* a server message
    /// arrived (`tx.send(event).await.is_err()`), so on an idle connection
    /// -- exactly this test's setup, the mock server never sends anything
    /// -- dropping the `Receiver` was never noticed and the task (and its
    /// socket) leaked forever. With `tokio::select!` racing `tx.closed()`
    /// against `read.next()`, dropping the receiver must be noticed
    /// promptly, and a `Close` frame sent to the server, with no server
    /// message needed to trigger it.
    #[tokio::test]
    async fn drops_receiver_promptly_without_waiting_for_a_server_message() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("local_addr");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept tcp connection");
            let ws_stream = tokio_tungstenite::accept_async(stream)
                .await
                .expect("complete ws handshake");
            let (_write, mut read) = ws_stream.split();
            // Deliberately send nothing -- an idle connection is the case
            // the pre-M3 code handled badly. Just wait for the client to
            // send a Close frame (or the connection to drop) in response
            // to its receiver being dropped.
            while let Some(Ok(msg)) = read.next().await {
                if matches!(msg, Message::Close(_)) {
                    return true; // client noticed and closed cleanly
                }
            }
            false // socket closed some other way (still fine, not hung)
        });

        let identity = ClientIdentity {
            client: "Test".to_string(),
            device: "test".to_string(),
            device_id: "test-device".to_string(),
            version: "0.0.0".to_string(),
        };
        let base_url = format!("http://{addr}");
        let rx = connect(&base_url, &identity, "test-token")
            .await
            .expect("connect to mock server");

        drop(rx);

        // Bounded wait: if the receiver-drop isn't noticed until the next
        // server message (which never comes here), this times out --
        // that's the exact regression M3 fixes.
        let saw_close = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server task must observe the client closing promptly, not hang")
            .expect("server task must not panic");
        assert!(
            saw_close,
            "client should send a Close frame once its receiver is dropped"
        );
    }
}
