//! PAN-123: an app action reaches the client that started the turn — and only it.
//!
//! These drive the real server on a temporary data directory: a real `/events`
//! socket per client, the real `BroadcastSink`, the real dispatch path for the
//! answer, and a real `RunContext` built exactly as `spawn` builds one for a turn.
//!
//! What they do **not** prove: that a provider harness asks for an app action. That
//! tool is the window-scoped host stack, which a headless process does not have, so
//! the request below is the entry a turn will call, not a call made by a model.

use std::{
    path::Path,
    sync::{
        atomic::AtomicBool,
        Arc,
    },
    time::Duration,
};

use futures_util::StreamExt;
use serde_json::{json, Value};

use super::{
    events::{BroadcastSink, Frame},
    live::{GatewaySurface, Live},
    spawn, GatewayHandle,
};
use crate::run_ctx::RunContext;

mod support {
    use super::*;

    pub async fn start(dir: &Path) -> GatewayHandle {
        let config = super::super::GatewayConfig {
            data_dir: dir.to_path_buf(),
            bind: "127.0.0.1:0".parse().unwrap(),
            token: "test-token".into(),
            static_dir: None,
            allowed_origins: super::super::config::default_origins(),
            upload_max: super::super::config::DEFAULT_UPLOAD_MAX,
            terminal_idle: super::super::config::DEFAULT_TERMINAL_IDLE,
            force_owner: false,
            approval_ttl: Some(super::super::config::DEFAULT_APPROVAL_TTL),
        };
        super::spawn(config).await.unwrap()
    }

    pub type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// Connects to `/events` with a query tail such as `&client=alpha`.
    pub async fn socket(addr: std::net::SocketAddr, query: &str) -> Socket {
        let url = format!("ws://{addr}/events?token=test-token{query}");
        let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket
    }

    /// The next frame, or `None` when nothing arrived in time.
    pub async fn next_frame(socket: &mut Socket, seconds: u64) -> Option<Value> {
        match tokio::time::timeout(Duration::from_secs(seconds), socket.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                Some(serde_json::from_str(&text).unwrap())
            }
            _ => None,
        }
    }

    /// rustls is built without a default provider, so the first HTTP client in a
    /// process installs one (the same call `cloud::http::client` makes).
    fn install_crypto_provider() {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
    }

    /// One RPC call, optionally naming the client that makes it.
    pub async fn rpc_as(
        handle: &GatewayHandle,
        command: &str,
        args: Value,
        client: Option<&str>,
    ) -> Value {
        install_crypto_provider();
        let mut request = reqwest::Client::new()
            .post(format!("http://{}/rpc", handle.addr))
            .header("Authorization", "Bearer test-token")
            .json(&json!({ "id": "1", "command": command, "args": args }));
        if let Some(client) = client {
            request = request.header("x-pandacode-client", client);
        }
        request.send().await.unwrap().json().await.unwrap()
    }

    /// The context a turn started by `client` runs with: this gateway's own sink, its
    /// own app-action registry and its own surface — what `spawn` builds for a run.
    /// The live registry is fresh because nothing here ends a turn; an app action
    /// never reads it.
    pub fn turn_context(handle: &GatewayHandle, client: &str) -> RunContext {
        let sink = handle.sink.clone();
        let dir = handle.data_dir.clone();
        RunContext::new(
            sink.clone(),
            dir.join("cache"),
            dir,
            handle.state.ui.clone(),
            Arc::new(GatewaySurface::new(Arc::new(Live::default()), sink)),
        )
        .for_client(Some(client))
    }
}

use support::*;

/// Proof 1. Two clients watch; the action addressed to `alpha` reaches `alpha`, and
/// `beta` receives nothing — asserted on the frame streams, not on a log line. Then
/// `beta` cannot answer it and `alpha` can.
#[tokio::test]
async fn an_app_action_reaches_the_client_that_started_the_turn_and_no_other() {
    let dir = tempfile::TempDir::new().unwrap();
    let handle = start(dir.path()).await;
    let mut alpha = socket(handle.addr, "&client=alpha").await;
    let mut beta = socket(handle.addr, "&client=beta").await;

    let run = turn_context(&handle, "alpha");
    assert_eq!(run.client(), Some("alpha"));
    let asking = tokio::spawn(async move {
        run.request_app_action(
            "s1",
            "open_picker",
            json!({ "kind": "file" }),
            Arc::new(AtomicBool::new(false)),
        )
        .await
    });

    let frame = next_frame(&mut alpha, 10)
        .await
        .expect("the addressed client received the action");
    assert_eq!(frame["event"], "agent-ui-action");
    assert_eq!(frame["client"], "alpha", "the frame names its owner");
    assert_eq!(frame["payload"]["action"], "open_picker");
    assert_eq!(frame["payload"]["sessionId"], "s1");
    let request_id = frame["payload"]["requestId"].as_str().unwrap().to_owned();

    assert!(
        next_frame(&mut beta, 2).await.is_none(),
        "a client that was not addressed must receive nothing"
    );

    // The answer belongs to the client the action was addressed to.
    let stolen = rpc_as(
        &handle,
        "resolve_agent_ui_action",
        json!({ "requestId": request_id, "result": "beta" }),
        Some("beta"),
    )
    .await;
    assert_eq!(stolen["ok"], false);
    assert_eq!(stolen["error"], "This app action belongs to another client.");

    // Nor can an unnamed caller take it: no client id must fail honestly, not be
    // treated as everybody.
    let anonymous = rpc_as(
        &handle,
        "resolve_agent_ui_action",
        json!({ "requestId": request_id, "result": "nobody" }),
        None,
    )
    .await;
    assert_eq!(anonymous["ok"], false);
    assert_eq!(anonymous["error"], "This app action belongs to another client.");

    let answered = rpc_as(
        &handle,
        "resolve_agent_ui_action",
        json!({ "requestId": request_id, "result": "alpha" }),
        Some("alpha"),
    )
    .await;
    assert_eq!(answered["ok"], true, "{answered}");
    assert_eq!(asking.await.unwrap().unwrap(), json!("alpha"));

    handle.stop().await;
}

/// Proof 2. The regression the ticket's literal reading would cause: a socket that
/// names a `target` stops receiving the session frames it also watches, while a
/// socket that names a `client` keeps receiving them. Identity is additive; a target
/// filter is not.
#[tokio::test]
async fn a_client_identity_keeps_the_session_frames_a_target_filter_would_drop() {
    let dir = tempfile::TempDir::new().unwrap();
    let handle = start(dir.path()).await;
    let mut watcher = socket(handle.addr, "&session=s1&client=alpha").await;
    let mut target_only = socket(handle.addr, "&session=s1&target=alpha").await;

    // An ordinary session frame, addressed to nobody.
    handle.emit("status", json!({ "sessionId": "s1", "status": "running" }));

    let frame = next_frame(&mut watcher, 5)
        .await
        .expect("a client identity does not hide the session it watches");
    assert_eq!(frame["payload"]["status"], "running");
    assert_eq!(frame["client"], Value::Null, "an ordinary frame is unaddressed");

    assert!(
        next_frame(&mut target_only, 2).await.is_none(),
        "the literal fix — subscribing with a target — is what loses this frame"
    );

    // ...while the target filter does still receive frames that carry its target,
    // which is why it is kept as a filter rather than repurposed as an identity.
    handle.sink.publish_to(
        "alpha",
        "agent-ui-action",
        json!({ "sessionId": "s1", "requestId": "r" }),
    );
    let targeted = next_frame(&mut target_only, 5)
        .await
        .expect("a target filter receives its own target");
    assert_eq!(targeted["target"], "alpha");

    handle.stop().await;
}

/// Proof 3. With no client attached the action fails at its deadline and names the
/// reason, and the run continues — the failure is a sentence, not a silence.
#[tokio::test]
async fn an_app_action_with_no_client_fails_at_its_deadline_and_names_the_reason() {
    let dir = tempfile::TempDir::new().unwrap();
    let handle = start(dir.path()).await;
    let run = turn_context(&handle, "alpha");
    let started = std::time::Instant::now();
    let error = run
        .request_app_action(
            "s1",
            "open_picker",
            json!({}),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
    let waited = started.elapsed();
    assert_eq!(error, "No client is attached to answer this app action.");
    assert!(
        waited >= Duration::from_millis(4_800) && waited < Duration::from_secs(9),
        "the no-client deadline is five seconds: {waited:?}"
    );
    handle.stop().await;
}

/// Proof 1's other half: a socket for one client is not evidence that a *different*
/// client is attached, so an action addressed to an absent client fails instead of
/// waiting on whoever happens to be listening.
#[tokio::test]
async fn another_clients_socket_does_not_make_an_absent_client_attached() {
    let dir = tempfile::TempDir::new().unwrap();
    let handle = start(dir.path()).await;
    let _beta = socket(handle.addr, "&client=beta").await;
    let run = turn_context(&handle, "alpha");
    let error = run
        .request_app_action(
            "s1",
            "open_picker",
            json!({}),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "No client is attached to answer this app action.");
    handle.stop().await;
}

/// The sink-level rule the proofs above rest on, without a server in the way.
#[test]
fn an_addressed_frame_is_delivered_by_identity_and_never_by_session() {
    use crate::gateway::events::Filter;
    let sink = BroadcastSink::new();
    let mut alpha = sink.subscribe(
        Filter {
            session: Some("s1".into()),
            client: Some("alpha".into()),
            ..Default::default()
        },
        None,
    );
    let mut beta = sink.subscribe(
        Filter {
            session: Some("s1".into()),
            client: Some("beta".into()),
            ..Default::default()
        },
        None,
    );
    let mut watcher = sink.subscribe(
        Filter {
            session: Some("s1".into()),
            ..Default::default()
        },
        None,
    );

    /// What one socket's queue holds right now.
    fn drain(
        rx: &mut tokio::sync::mpsc::Receiver<Frame>,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            seen.push(frame.event.clone());
        }
        seen
    }

    assert!(sink.has_client("alpha"));
    assert!(!sink.has_client("gamma"));

    sink.publish_to_client("alpha", "agent-ui-action", json!({ "sessionId": "s1" }));
    assert_eq!(drain(&mut alpha.frames), ["agent-ui-action"]);
    assert!(drain(&mut beta.frames).is_empty(), "beta shares the session, not the identity");
    assert!(
        drain(&mut watcher.frames).is_empty(),
        "a session watcher is not the client the action was addressed to"
    );

    // An unaddressed frame reaches every socket watching that session, as before.
    sink.publish("status", json!({ "sessionId": "s1" }));
    assert_eq!(drain(&mut alpha.frames), ["status"]);
    assert_eq!(drain(&mut beta.frames), ["status"]);
    assert_eq!(drain(&mut watcher.frames), ["status"]);
}
