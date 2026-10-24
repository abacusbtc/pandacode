//! End-to-end HTTP tests against a real gateway on a temporary data directory.
//!
//! Gated on the `gateway` feature so a build without it compiles exactly what it
//! compiled before (`cargo test --lib`, `cargo clippy --all-targets`).
#![cfg(feature = "gateway")]

use std::path::Path;

use futures_util::StreamExt;
use pandacode_lib::gateway::{config, spawn, GatewayHandle, GatewayConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The crate builds rustls without a default provider (`rustls-no-provider`), so
/// any process that builds a reqwest client installs one first — the same call
/// `cloud::http::client` makes for the app.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// A config for a caller-held temp directory: `tempfile::TempDir` deletes its
/// directory on drop, so the test body owns it and only the path travels here.
fn test_config(data_dir: &Path, bind: &str) -> GatewayConfig {
    GatewayConfig {
        data_dir: data_dir.to_path_buf(),
        bind: bind.parse().unwrap(),
        token: "test-token".into(),
        static_dir: None,
        allowed_origins: config::default_origins(),
        upload_max: config::DEFAULT_UPLOAD_MAX,
        terminal_idle: config::DEFAULT_TERMINAL_IDLE,
        force_owner: false,
        approval_ttl: Some(config::DEFAULT_APPROVAL_TTL),
    }
}

async fn start(data_dir: &Path) -> GatewayHandle {
    spawn(test_config(data_dir, "127.0.0.1:0")).await.unwrap()
}

/// The refusal message from a spawn that had to fail, without requiring the
/// handle to be printable.
async fn refusal(config: GatewayConfig) -> String {
    match spawn(config).await {
        Ok(_) => panic!("the gateway started when it should have refused"),
        Err(error) => error.to_string(),
    }
}

async fn rpc(handle: &GatewayHandle, command: &str, args: Value) -> Value {
    let response = reqwest::Client::new()
        .post(format!("http://{}/rpc", handle.addr))
        .header("Authorization", "Bearer test-token")
        .json(&json!({ "id": "1", "command": command, "args": args }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{command} did not answer 200");
    response.json().await.unwrap()
}

async fn call(handle: &GatewayHandle, command: &str, args: Value) -> Value {
    let body = rpc(handle, command, args).await;
    assert_eq!(body["ok"], true, "{command} failed: {body}");
    body["result"].clone()
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Connects to `/events`, asserting the handshake succeeded.
async fn events_socket(addr: std::net::SocketAddr, query: &str) -> Socket {
    let url = format!("ws://{addr}/events?token=test-token{query}");
    let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
}

/// The next frame, failing the test rather than hanging the suite.
async fn next_frame(socket: &mut Socket) -> Value {
    let message = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
        .await
        .expect("a frame arrived")
        .expect("the socket stayed open")
        .expect("the frame was a message");
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

/// Asserts that nothing else arrives: the socket is quiet, not merely slow.
async fn no_more_frames(socket: &mut Socket) {
    if let Ok(Some(frame)) = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        socket.next(),
    )
    .await
    {
        panic!("an unexpected frame arrived: {frame:?}");
    }
}

#[tokio::test]
async fn healthz_returns_ok() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let url = format!("http://{}/healthz", handle.addr);
    let response = reqwest::get(url).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    handle.stop().await;
}

#[tokio::test]
async fn rpc_requires_bearer_token() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let url = format!("http://{}/rpc", handle.addr);
    let rpc = json!({ "id": "1", "command": "list_projects", "args": {} });

    let denied = reqwest::Client::new()
        .post(&url)
        .json(&rpc)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    assert_eq!(denied.text().await.unwrap(), "unauthorized");

    let allowed = reqwest::Client::new()
        .post(&url)
        .header("Authorization", "Bearer test-token")
        .json(&rpc)
        .send()
        .await
        .unwrap();
    // The token carried the call past auth and the dispatcher answered it.
    assert_eq!(allowed.status(), 200);
    let body: Value = allowed.json().await.unwrap();
    assert_eq!(body["ok"], true, "{body}");
    handle.stop().await;
}

#[tokio::test]
async fn preflight_is_answered_before_auth() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let url = format!("http://{}/rpc", handle.addr);
    let client = reqwest::Client::new();

    // A browser preflight carries no Authorization header, so auth must never
    // see it: the whole cross-origin client fails with an opaque error otherwise.
    let allowed = client
        .request(reqwest::Method::OPTIONS, &url)
        .header("Origin", "http://tauri.localhost")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "authorization,content-type")
        .send()
        .await
        .unwrap();
    // tower-http answers a preflight with 200 rather than 204; RFC 9110 accepts
    // any 2xx, and what the browser needs is the reflected origin plus no 401.
    assert_eq!(allowed.status(), 200);
    assert_eq!(
        allowed
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("http://tauri.localhost")
    );
    assert_eq!(
        allowed
            .headers()
            .get("access-control-allow-methods")
            .and_then(|value| value.to_str().ok()),
        Some("GET,POST")
    );

    // An origin nobody named is not reflected back.
    let stranger = client
        .request(reqwest::Method::OPTIONS, &url)
        .header("Origin", "https://not-the-app.example")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert!(stranger
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    handle.stop().await;
}

#[test]
fn public_bind_without_token_is_refused() {
    let error = config::validate("0.0.0.0:8443".parse().unwrap(), false).unwrap_err();
    assert!(error.contains("refusing to bind"), "{error}");
}

#[test]
fn loopback_or_an_explicit_token_may_bind_anywhere() {
    assert!(config::validate("127.0.0.1:47399".parse().unwrap(), false).is_ok());
    assert!(config::validate("0.0.0.0:8443".parse().unwrap(), true).is_ok());
}

#[test]
fn args_carry_origins_and_the_public_bind_rule() {
    let parsed = config::from_args(
        ["--token", "chosen", "--allow-origin", "https://panda.lan"].map(String::from),
    )
    .unwrap();
    assert_eq!(parsed.bind, "127.0.0.1:47399".parse().unwrap());
    assert_eq!(parsed.allowed_origins, vec!["https://panda.lan".to_string()]);

    // A public bind with no explicit token is refused before any token is written.
    assert!(config::from_args(["--host", "0.0.0.0"].map(String::from)).is_err());
}

#[tokio::test]
async fn add_and_list_projects_round_trip_through_the_real_database() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;

    let created = call(
        &handle,
        "add_project",
        json!({ "path": project.path().to_str().unwrap() }),
    )
    .await;
    assert_eq!(
        created["path"].as_str().unwrap(),
        project.path().to_str().unwrap()
    );

    let listed = call(&handle, "list_projects", json!({})).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["id"], created["id"]);
    assert!(data.path().join("kingcode.sqlite3").exists());
    handle.stop().await;
}

#[tokio::test]
async fn remove_project_takes_a_project_out_of_the_list() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;
    let created = call(
        &handle,
        "add_project",
        json!({ "path": project.path().to_str().unwrap() }),
    )
    .await;

    let removed = call(
        &handle,
        "remove_project",
        json!({ "projectId": created["id"] }),
    )
    .await;
    assert_eq!(removed, Value::Null);
    let listed = call(&handle, "list_projects", json!({})).await;
    assert!(listed.as_array().unwrap().is_empty());
    handle.stop().await;
}

#[tokio::test]
async fn unsupported_command_is_reported_not_hung() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let body = rpc(&handle, "choose_folder", json!({})).await;
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["error"], "unsupported_command: choose_folder");
    handle.stop().await;
}

#[tokio::test]
async fn the_message_queue_round_trips_over_rpc() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;
    let created = call(
        &handle,
        "add_project",
        json!({ "path": project.path().to_str().unwrap() }),
    )
    .await;
    let session = call(
        &handle,
        "create_session",
        json!({ "projectId": created["id"], "provider": "claude", "model": "sonnet" }),
    )
    .await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let first = call(
        &handle,
        "queue_add",
        json!({ "sessionId": session_id, "text": "first" }),
    )
    .await;
    let second = call(
        &handle,
        "queue_add",
        json!({ "sessionId": session_id, "text": "second" }),
    )
    .await;
    let listed = call(&handle, "queue_list", json!({ "sessionId": session_id })).await;
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["text"], "first");

    let reordered = call(
        &handle,
        "queue_reorder",
        json!({ "sessionId": session_id, "ids": [second["id"], first["id"]] }),
    )
    .await;
    assert_eq!(reordered[0]["id"], second["id"], "the order the client asked for");

    let edited = call(
        &handle,
        "queue_update",
        json!({ "id": first["id"], "text": "first, edited" }),
    )
    .await;
    assert_eq!(edited["text"], "first, edited");

    call(&handle, "queue_remove", json!({ "id": first["id"] })).await;
    let taken = call(&handle, "queue_take", json!({ "id": second["id"] })).await;
    assert_eq!(taken["text"], "second");
    // Taking delivers once; restoring puts the same entry back at the front.
    let restored = call(&handle, "queue_restore", json!({ "entry": taken })).await;
    assert_eq!(restored["text"], "second");
    let listed = call(&handle, "queue_list", json!({ "sessionId": session_id })).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    handle.stop().await;
}

#[tokio::test]
async fn turn_commands_answer_with_the_commands_own_error() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;
    let created = call(
        &handle,
        "add_project",
        json!({ "path": project.path().to_str().unwrap() }),
    )
    .await;
    let session = call(
        &handle,
        "create_session",
        json!({ "projectId": created["id"], "provider": "claude", "model": "sonnet" }),
    )
    .await;
    let session_id = session["id"].as_str().unwrap().to_string();

    // Stopping a task with no turn running is the no-op the command is.
    let stopped = call(&handle, "stop_session", json!({ "sessionId": session_id })).await;
    assert_eq!(stopped, Value::Null);

    // Steering one keeps the message queued, in the desktop's words.
    let body = rpc(
        &handle,
        "steer_task",
        json!({ "sessionId": session_id, "text": "go" }),
    )
    .await;
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(
        body["error"],
        "The agent is not running; the message stays queued."
    );

    // An approval for a turn that is not running says so rather than hanging.
    let body = rpc(
        &handle,
        "respond_approval",
        json!({ "sessionId": session_id, "requestId": "r1", "approved": true }),
    )
    .await;
    assert_eq!(body["error"], "This agent is no longer running.");

    // A send and a start reach the shared launch path: an unknown task fails on
    // the store, not on the command's name, and nothing is launched. A real send
    // would start a harness turn, which is task 12's end-to-end proof.
    for command in ["send_message", "start_task"] {
        let body = rpc(
            &handle,
            command,
            json!({ "sessionId": "no-such-task", "text": "hi" }),
        )
        .await;
        assert_eq!(body["ok"], false, "{command}: {body}");
        assert_eq!(body["error"], "Conversation not found.", "{command}: {body}");
    }
    handle.stop().await;
}

#[tokio::test]
async fn the_live_registry_is_empty_and_advertised() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;

    // Nothing is running, so nothing is live — and a task that exists is not live
    // either, because the registry is written by launches, not read from rows.
    let listed = call(&handle, "gateway_live_tasks", json!({})).await;
    assert_eq!(listed, json!([]));

    let created = call(
        &handle,
        "add_project",
        json!({ "path": project.path().to_str().unwrap() }),
    )
    .await;
    call(
        &handle,
        "create_session",
        json!({ "projectId": created["id"], "provider": "claude", "model": "sonnet" }),
    )
    .await;
    let listed = call(&handle, "gateway_live_tasks", json!({})).await;
    assert_eq!(listed, json!([]), "an idle task is not a live task");

    let url = format!("http://{}/capabilities", handle.addr);
    let body: Value = reqwest::Client::new()
        .get(&url)
        .header("Authorization", "Bearer test-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let commands: Vec<&str> = body["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|command| command.as_str())
        .collect();
    assert!(commands.contains(&"gateway_live_tasks"));
    handle.stop().await;
}

#[test]
fn approval_ttl_is_parsed_and_zero_disables_auto_deny() {
    let dir = tempfile::tempdir().unwrap();
    let parse = |extra: &[&str]| {
        let mut args: Vec<String> = vec![
            "--data-dir".into(),
            dir.path().to_str().unwrap().into(),
            "--token".into(),
            "test-token".into(),
        ];
        args.extend(extra.iter().map(|value| value.to_string()));
        config::from_args(args)
    };
    assert_eq!(
        parse(&[]).unwrap().approval_ttl,
        Some(std::time::Duration::from_secs(3600)),
        "one hour by default"
    );
    assert_eq!(
        parse(&["--approval-ttl", "30m"]).unwrap().approval_ttl,
        Some(std::time::Duration::from_secs(1800))
    );
    assert_eq!(
        parse(&["--approval-ttl", "45s"]).unwrap().approval_ttl,
        Some(std::time::Duration::from_secs(45))
    );
    assert_eq!(
        parse(&["--approval-ttl", "0"]).unwrap().approval_ttl,
        None,
        "0 is the explicit never-auto-deny choice"
    );
    assert!(parse(&["--approval-ttl", "soon"]).is_err(), "a bad duration is refused");
}

#[tokio::test]
async fn a_remote_client_picks_a_real_model_and_creates_a_task() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;

    // What a client does first: ask what exists.
    let providers = call(&handle, "list_providers", json!({})).await;
    let providers = providers.as_array().expect("a list of providers");
    assert!(!providers.is_empty(), "the host installs at least one harness");
    let provider = providers
        .iter()
        .find(|entry| entry["installed"] == true)
        .unwrap_or(&providers[0]);
    let provider_id = provider["id"].as_str().unwrap().to_string();

    // Then that provider's models. The answer may carry them, or the model call may,
    // depending on whether the provider ships a static catalog.
    let models = rpc(&handle, "list_provider_models", json!({"provider": provider_id})).await;
    let model_id = models["result"]
        .as_array()
        .and_then(|list| list.first())
        .and_then(|model| model["id"].as_str())
        .or_else(|| {
            provider["models"]
                .as_array()
                .and_then(|list| list.first())
                .and_then(|model| model["id"].as_str())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("{provider_id} names no model: {models}"));

    let created = call(
        &handle,
        "add_project",
        json!({ "path": project.path().to_str().unwrap() }),
    )
    .await;
    let session = call(
        &handle,
        "create_session",
        json!({
            "projectId": created["id"],
            "provider": provider_id,
            "model": model_id,
        }),
    )
    .await;
    assert_eq!(session["provider"], provider_id, "the provider the client picked");
    assert_eq!(session["model"], model_id, "the model the client picked");

    // Read it back through the served list, not from the create answer.
    let listed = call(&handle, "list_sessions", json!({})).await;
    let read_back = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == session["id"])
        .expect("the new task is listed");
    assert_eq!(read_back["provider"], provider_id);
    assert_eq!(read_back["model"], model_id);

    // Honest validation: the provider is checked, the model is a string the desktop
    // never checks against the catalog either. Both halves are asserted so nobody
    // reads more into this proof than it holds.
    let refused = rpc(
        &handle,
        "create_session",
        json!({ "projectId": created["id"], "provider": "no-such-harness", "model": "x" }),
    )
    .await;
    assert_eq!(refused["error"], "Unknown provider.");
    let unchecked = call(
        &handle,
        "create_session",
        json!({ "projectId": created["id"], "provider": provider_id, "model": "definitely-not-a-model" }),
    )
    .await;
    assert_eq!(
        unchecked["model"], "definitely-not-a-model",
        "a model id is not validated against the catalog, on the desktop or here"
    );
    handle.stop().await;
}

#[tokio::test]
async fn a_second_gateway_on_the_same_data_directory_is_refused() {    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let error = refusal(test_config(dir.path(), "127.0.0.1:0")).await;
    // The message has to name the directory and the holder, because the same
    // sentence is what the desktop app shows when the gateway is the one running.
    assert!(error.contains("is in use by another process"), "{error}");
    assert!(error.contains(&dir.path().display().to_string()), "{error}");
    handle.stop().await;

    // The kernel released the lock with the process, so the next start is clean.
    let again = start(dir.path()).await;
    again.stop().await;
}

#[tokio::test]
async fn force_owner_cannot_take_a_live_lock() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let mut config = test_config(dir.path(), "127.0.0.1:0");
    config.force_owner = true;
    let error = refusal(config).await;
    assert!(error.contains("is in use by another process"), "{error}");
    assert!(
        error.contains("--force-owner does not take a live lock"),
        "{error}"
    );
    handle.stop().await;
}

#[tokio::test]
async fn websocket_clients_receive_emitted_events() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let url = format!("ws://{}/events?token=test-token", handle.addr);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    // The exact bytes a streaming text delta puts on the wire. The frame must not
    // wrap, rename or re-nest them: a client reads these fields by name.
    let delta = json!({
        "sessionId": "s1",
        "kind": "text",
        "text": "hi",
        "requestId": null,
        "data": null,
    });
    handle.emit("agent-event", delta.clone());

    let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    assert_eq!(frame["event"], "agent-event");
    assert_eq!(frame["payload"], delta, "payload was rewritten in transit");
    // The envelope adds a number and nothing else, and never reaches inside the
    // payload: a client reads these fields by name.
    assert!(frame["seq"].as_u64().unwrap() >= 1, "every frame is numbered");
    let mut keys: Vec<&String> = frame.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["event", "payload", "seq"]);
    handle.stop().await;
}

#[tokio::test]
async fn a_slow_reader_is_told_what_it_missed_and_can_ask_for_it() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let mut socket = events_socket(handle.addr, "").await;

    // A burst the socket cannot keep up with. The emit loop never yields, so the
    // socket's queue fills and everything after it is a hole — and the emitter
    // neither awaits nor waits for the socket to drain.
    let text = "x".repeat(4096);
    let total = 1500u64;
    for index in 0..total {
        handle.emit(
            "agent-event",
            json!({ "sessionId": "s1", "kind": "text", "text": text, "index": index }),
        );
    }

    // The first thing the client learns is the range it missed, not a quiet jump.
    let lagged = next_frame(&mut socket).await;
    assert_eq!(lagged["event"], "lagged", "a hole must be announced: {lagged}");
    let from = lagged["payload"]["fromSeq"].as_u64().unwrap();
    let to = lagged["payload"]["toSeq"].as_u64().unwrap();
    assert!(from > 1 && to > from, "a named range: {lagged}");

    // What did arrive is the frames before the hole, in order and once each.
    let mut live = Vec::new();
    while live.len() < (from - 1) as usize {
        let frame = next_frame(&mut socket).await;
        assert_eq!(frame["event"], "agent-event");
        live.push(frame["seq"].as_u64().unwrap());
    }
    assert_eq!(
        live,
        (1..from).collect::<Vec<_>>(),
        "no gap and no duplicate before the announced hole"
    );

    // The hole itself is still in the ring, so resuming from just before it returns
    // exactly the missed frames — no hole, no duplicate, in order.
    let mut replay = events_socket(handle.addr, &format!("&since={}", from - 1)).await;
    let mut recovered = Vec::new();
    while recovered.len() < (to - from + 1) as usize {
        let frame = next_frame(&mut replay).await;
        assert_ne!(frame["event"], "lagged", "the ring still had this range");
        recovered.push(frame["seq"].as_u64().unwrap());
    }
    assert_eq!(
        recovered,
        (from..=to).collect::<Vec<_>>(),
        "the missed frames, in order, once each"
    );
    handle.stop().await;
}

#[tokio::test]
async fn two_sessions_never_leak_into_each_other() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let mut a = events_socket(handle.addr, "&session=a").await;
    let mut b = events_socket(handle.addr, "&session=b").await;

    for index in 0..10 {
        handle.emit(
            "agent-event",
            json!({ "sessionId": "a", "kind": "text", "text": format!("a{index}") }),
        );
        handle.emit(
            "agent-event",
            json!({ "sessionId": "b", "kind": "text", "text": format!("b{index}") }),
        );
    }

    for index in 0..10 {
        let frame = next_frame(&mut a).await;
        assert_eq!(frame["payload"]["sessionId"], "a");
        assert_eq!(frame["payload"]["text"], format!("a{index}"));
    }
    for index in 0..10 {
        let frame = next_frame(&mut b).await;
        assert_eq!(frame["payload"]["sessionId"], "b");
        assert_eq!(frame["payload"]["text"], format!("b{index}"));
    }
    // Neither socket saw the other's frames, and no frame arrived twice.
    no_more_frames(&mut a).await;
    no_more_frames(&mut b).await;
    handle.stop().await;
}

#[tokio::test]
async fn a_terminal_filter_isolates_one_shell() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let mut socket = events_socket(handle.addr, "&terminal=t1").await;

    handle.emit("terminal-event", json!({ "terminalId": "t1", "data": "one" }));
    handle.emit("terminal-event", json!({ "terminalId": "t2", "data": "two" }));
    handle.emit("agent-event", json!({ "sessionId": "s1", "kind": "text", "text": "x" }));

    let frame = next_frame(&mut socket).await;
    assert_eq!(frame["event"], "terminal-event");
    assert_eq!(frame["payload"]["terminalId"], "t1");
    assert_eq!(frame["payload"]["data"], "one");
    // The other shell's output, and another session's agent event, are not this
    // socket's business.
    no_more_frames(&mut socket).await;
    handle.stop().await;
}

#[tokio::test]
async fn a_bad_since_is_a_client_error_and_new_parameters_are_still_guarded() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;

    // A since that is not a frame number is refused, rather than silently replayed
    // from the start as if the client had asked for everything.
    let error = tokio_tungstenite::connect_async(format!(
        "ws://{}/events?token=test-token&since=yesterday",
        handle.addr
    ))
    .await
    .unwrap_err();
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 400, "a bad since is the client's error")
        }
        other => panic!("expected an HTTP refusal, got {other}"),
    }

    // The new parameters do not create a way around the token.
    let denied = reqwest::Client::new()
        .get(format!(
            "http://{}/events?session=a&terminal=t1",
            handle.addr
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    handle.stop().await;
}

#[tokio::test]
async fn the_sessions_changed_frame_carries_a_null_payload() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let url = format!("ws://{}/events?token=test-token", handle.addr);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    // Every sessions-changed call site passes (). A payload that turned into {} or
    // a struct would still arrive and still be ignored, until a client read it.
    handle.emit("sessions-changed", ());
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    assert_eq!(frame["event"], "sessions-changed");
    assert_eq!(frame["payload"], Value::Null);
    handle.stop().await;
}

#[tokio::test]
async fn the_events_socket_refuses_a_missing_or_wrong_token() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;

    let bare = format!("ws://{}/events", handle.addr);
    assert!(
        tokio_tungstenite::connect_async(bare).await.is_err(),
        "the socket accepted a handshake with no token"
    );
    let wrong = format!("ws://{}/events?token=nope", handle.addr);
    assert!(
        tokio_tungstenite::connect_async(wrong).await.is_err(),
        "the socket accepted a wrong token"
    );
    handle.stop().await;
}

#[tokio::test]
async fn a_lock_that_cannot_be_taken_is_refused_not_warned() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    // A lock we cannot even open is the "I cannot promise I am the only writer"
    // case: the gateway must refuse rather than run unprotected.
    let path = dir.path().join("kingcode.lock");
    std::fs::write(&path, "").unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).unwrap();

    let error = refusal(test_config(dir.path(), "127.0.0.1:0")).await;
    assert!(error.contains("cannot open"), "{error}");

    // And --force-owner is not a way around an unprovable lock either.
    let mut forced = test_config(dir.path(), "127.0.0.1:0");
    forced.force_owner = true;
    let error = refusal(forced).await;
    assert!(error.contains("cannot open"), "{error}");
}

#[tokio::test]
async fn the_shared_lock_refuses_for_the_desktop_too() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    // This is the helper the desktop app calls in setup(), exercised directly:
    // launching the real window is not part of this suite.
    let handle = start(dir.path()).await;
    assert!(matches!(
        config::claim(dir.path(), false),
        config::Claim::Busy(_)
    ));
    handle.stop().await;

    let claimed = config::claim(dir.path(), false);
    assert!(matches!(claimed, config::Claim::Held(_)), "claim after stop");
}

#[tokio::test]
async fn a_lock_file_from_a_dead_process_does_not_block() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    let mut child = child;
    child.wait().unwrap();
    // A kernel lock cannot be inherited from a file's contents, so a leftover
    // PID is diagnostics, not state.
    std::fs::write(dir.path().join("kingcode.lock"), format!("{dead}\n")).unwrap();

    let handle = start(dir.path()).await;
    let listed = call(&handle, "list_projects", json!({})).await;
    assert!(listed.as_array().unwrap().is_empty());
    handle.stop().await;
}

#[test]
fn the_lock_directory_is_the_identifier_tauri_uses() {
    // Both processes only guard the same database if they compute the same
    // directory, and that directory is the bundle identifier under the data base.
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tauri.conf.json"
    ))
    .unwrap();
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        parsed["identifier"].as_str().unwrap(),
        config::APP_DIR,
        "tauri.conf.json identifier and the gateway's APP_DIR disagree, so the lock and the database would sit in different directories"
    );

    let base = std::path::PathBuf::from("/example/data");
    assert_eq!(
        config::data_dir_under(base),
        std::path::PathBuf::from("/example/data").join(config::APP_DIR)
    );
}

#[tokio::test]
async fn a_database_from_a_newer_build_is_refused() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kingcode.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.pragma_update(None, "user_version", 999).unwrap();
    drop(connection);

    let error = refusal(test_config(dir.path(), "127.0.0.1:0")).await;
    assert!(error.contains("schema version 999"), "{error}");
    assert!(error.contains("Update PandaCode"), "{error}");
}

#[tokio::test]
async fn capabilities_and_sessions_come_from_the_real_schema() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;

    let status = call(&handle, "gateway_status", json!({})).await;
    let commands = status["commands"].as_array().unwrap();
    assert!(commands.iter().any(|command| command == "create_session"));
    assert!(commands.iter().any(|command| command == "get_messages"));

    let created = call(
        &handle,
        "add_project",
        json!({ "path": project.path().to_str().unwrap() }),
    )
    .await;
    let project_id = created["id"].as_str().unwrap();
    let session = call(
        &handle,
        "create_session",
        json!({ "projectId": project_id, "provider": "claude", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(session["projectId"], project_id);

    let sessions = call(&handle, "list_sessions", json!({})).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    let session_id = sessions[0]["id"].as_str().unwrap();

    let messages = call(&handle, "get_messages", json!({ "sessionId": session_id })).await;
    assert!(messages["messages"].as_array().unwrap().is_empty());
    assert_eq!(messages["hasOlder"], false);

    // The optional filter narrows to one project instead of listing everything.
    let elsewhere = call(&handle, "list_sessions", json!({ "projectId": "not-a-project" })).await;
    assert!(elsewhere.as_array().unwrap().is_empty());
    handle.stop().await;
}

#[tokio::test]
async fn capabilities_come_from_the_table_the_dispatcher_answers() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let url = format!("http://{}/capabilities", handle.addr);

    let denied = reqwest::get(&url).await.unwrap();
    assert_eq!(denied.status(), 401);

    let response = reqwest::Client::new()
        .get(&url)
        .header("Authorization", "Bearer test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let commands: Vec<&str> = body["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|command| command.as_str())
        .collect();
    assert!(commands.contains(&"list_projects"));

    // A command listed twice would mean the table and the endpoint disagree about
    // what exists, so the list itself is part of the pin.
    let mut unique = commands.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), commands.len(), "duplicate command advertised: {commands:?}");

    // Everything advertised is answered: a command short of arguments fails on
    // the arguments, never on its name.
    for command in &commands {
        let body = rpc(&handle, command, json!({})).await;
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            !error.starts_with("unsupported_command"),
            "{command} is advertised but unanswered: {error}"
        );
    }

    // And something not advertised is refused by name. Teams are M2 scope that a
    // gateway does not serve, so `architect_start` is the standing example.
    let body = rpc(&handle, "architect_start", json!({})).await;
    assert_eq!(body["error"], "unsupported_command: architect_start");
    handle.stop().await;
}

#[tokio::test]
async fn lists_directories_on_the_server_filesystem() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("alpha")).unwrap();
    std::fs::create_dir(root.path().join("beta")).unwrap();
    std::fs::write(root.path().join("file.txt"), "x").unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let handle = start(data.path()).await;

    let listing = call(
        &handle,
        "gateway_list_dirs",
        json!({ "path": root.path().to_str().unwrap() }),
    )
    .await;

    // Folders only: a file is not a folder a project could be opened in, and a
    // dot directory stays hidden until asked for.
    let entries = listing["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    assert!(entries.iter().all(|entry| entry["isDir"] == true));
    assert_eq!(listing["path"], root.path().to_str().unwrap());
    assert_eq!(
        listing["parent"],
        root.path().parent().unwrap().to_str().unwrap()
    );

    // The returned path is what the client appends entry names to, so a trailing
    // slash is normalised away rather than doubling up in the next request.
    let slashed = call(
        &handle,
        "gateway_list_dirs",
        json!({ "path": format!("{}/", root.path().display()) }),
    )
    .await;
    assert_eq!(slashed["path"], root.path().to_str().unwrap());

    let hidden = call(
        &handle,
        "gateway_list_dirs",
        json!({ "path": root.path().to_str().unwrap(), "showHidden": true }),
    )
    .await;
    // Sorted case-insensitively, so the caller can render the list as received.
    let names: Vec<&str> = hidden["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec![".git", "alpha", "beta"]);
    handle.stop().await;
}

#[tokio::test]
async fn a_listing_path_the_server_cannot_use_is_refused() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "x").unwrap();
    let handle = start(data.path()).await;

    // A relative path is ambiguous on a remote server: it would resolve against
    // the gateway's own working directory, which is not the client's view.
    let body = rpc(&handle, "gateway_list_dirs", json!({ "path": "src" })).await;
    assert_eq!(body["error"], "not_a_directory: src");

    let missing = root.path().join("does-not-exist");
    let body = rpc(
        &handle,
        "gateway_list_dirs",
        json!({ "path": missing.to_str().unwrap() }),
    )
    .await;
    assert_eq!(
        body["error"],
        format!("not_a_directory: {}", missing.display())
    );

    // A regular file is not a directory either, and says so the same way.
    let file = root.path().join("file.txt");
    let body = rpc(
        &handle,
        "gateway_list_dirs",
        json!({ "path": file.to_str().unwrap() }),
    )
    .await;
    assert_eq!(body["error"], format!("not_a_directory: {}", file.display()));
    handle.stop().await;
}

#[tokio::test]
async fn an_omitted_listing_path_starts_at_the_server_home() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;

    let omitted = call(&handle, "gateway_list_dirs", json!({})).await;
    // Explicit null and an omitted key mean the same thing, so a client that
    // always sends the field gets the same starting point.
    let null = call(&handle, "gateway_list_dirs", json!({ "path": null })).await;
    assert_eq!(omitted["path"], null["path"]);

    if let Some(home) = std::env::var_os("HOME") {
        assert_eq!(omitted["path"], home.to_string_lossy().as_ref());
        assert!(omitted["entries"].is_array());
    }
    handle.stop().await;
}

#[tokio::test]
async fn directory_listing_is_advertised_and_answered() {
    install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let handle = start(data.path()).await;

    let response = reqwest::Client::new()
        .get(format!("http://{}/capabilities", handle.addr))
        .header("Authorization", "Bearer test-token")
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    let commands: Vec<&str> = body["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|command| command.as_str())
        .collect();
    // The other direction of the pin: a command the table answers must be
    // advertised, or a client hides a surface the server supports.
    assert!(
        commands.contains(&"gateway_list_dirs"),
        "gateway_list_dirs answers but is not advertised: {commands:?}"
    );

    let listing = call(
        &handle,
        "gateway_list_dirs",
        json!({ "path": root.path().to_str().unwrap() }),
    )
    .await;
    assert_eq!(listing["entries"].as_array().unwrap().len(), 0);
    handle.stop().await;
}

#[tokio::test]
async fn serves_index_for_unknown_routes_and_types_assets() {
    install_crypto_provider();
    let dist = tempfile::tempdir().unwrap();
    std::fs::write(dist.path().join("index.html"), "<html>pandacode</html>").unwrap();
    std::fs::write(dist.path().join("app.js"), "console.log(1)").unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut config = test_config(data.path(), "127.0.0.1:0");
    config.static_dir = Some(dist.path().to_path_buf());
    let handle = spawn(config).await.unwrap();
    let base = format!("http://{}", handle.addr);

    // A client route is the shell, and no token was needed to fetch it.
    let deep = reqwest::get(format!("{base}/threads/42")).await.unwrap();
    assert_eq!(deep.status(), 200);
    assert!(deep.text().await.unwrap().contains("pandacode"));

    let js = reqwest::get(format!("{base}/app.js")).await.unwrap();
    assert_eq!(
        js.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );

    // Nothing escapes the build directory: an encoded traversal is the shell.
    let escape = reqwest::get(format!("{base}/%2e%2e%2fCargo.toml"))
        .await
        .unwrap();
    let body = escape.text().await.unwrap();
    assert!(body.contains("pandacode"), "{body}");
    assert!(!body.contains("[package]"), "{body}");

    // The API stays guarded while the files are not.
    let unguarded = reqwest::Client::new()
        .post(format!("{base}/rpc"))
        .json(&json!({ "id": "1", "command": "list_projects", "args": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(unguarded.status(), 401);

    // A write to a client route is a wrong method, not a missing route: 200 with
    // the shell would tell a POST its body had been accepted somewhere.
    let write = reqwest::Client::new()
        .post(format!("{base}/threads/42"))
        .send()
        .await
        .unwrap();
    assert_eq!(write.status(), 405);
    assert_eq!(write.headers()["allow"], "GET, HEAD");
    handle.stop().await;
}

#[tokio::test]
async fn a_differently_spelled_rpc_path_is_still_guarded() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let rpc = json!({ "id": "1", "command": "list_projects", "args": {} });
    // axum does not route this spelling to rpc::handle, so this pins the auth
    // decision (a doubled or trailing slash cannot walk around the token), not a
    // live bypass.
    let response = reqwest::Client::new()
        .post(format!("http://{}/rpc/", handle.addr))
        .json(&rpc)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    let doubled = reqwest::Client::new()
        .post(format!("http://{}//rpc", handle.addr))
        .json(&rpc)
        .send()
        .await
        .unwrap();
    assert_eq!(doubled.status(), 401);
    handle.stop().await;
}

#[tokio::test]
async fn an_oversized_rpc_body_is_refused_with_413() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    // One byte past the 64 MB the router allows; the JSON wrapper adds a little.
    let padding = "x".repeat(64 * 1024 * 1024);
    let response = reqwest::Client::new()
        .post(format!("http://{}/rpc", handle.addr))
        .header("Authorization", "Bearer test-token")
        .json(&json!({
            "id": "1",
            "command": "list_projects",
            "args": { "padding": padding },
        }))
        .send()
        .await
        .unwrap();
    // 413, not 400 from the JSON rejection and not 500 from a buffered body.
    assert_eq!(response.status(), 413);
    handle.stop().await;
}

#[tokio::test]
async fn a_large_body_under_the_cap_still_reaches_the_dispatcher() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    // Well over the 2 MB default this router no longer inherits, and far under
    // the cap it sets, so the boundary is the coded one rather than a default.
    let padding = "x".repeat(4 * 1024 * 1024);
    let body = rpc(&handle, "list_projects", json!({ "padding": padding })).await;
    assert_eq!(body["ok"], true, "{body:?}");
    handle.stop().await;
}

#[tokio::test]
async fn unknown_routes_are_404_without_a_built_frontend() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let response = reqwest::get(format!("http://{}/threads/42", handle.addr))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.text().await.unwrap(), "not found");
    handle.stop().await;
}

// ---- uploads (M2 task 7, PAN-108) ------------------------------------------

/// The same gateway with a cap small enough for a test to cross it.
async fn start_with_cap(data_dir: &Path, upload_max: usize) -> GatewayHandle {
    let mut config = test_config(data_dir, "127.0.0.1:0");
    config.upload_max = upload_max;
    spawn(config).await.unwrap()
}

/// `encodeURIComponent`, so a test can name a file anything at all.
fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*'
            | b'\'' | b'(' | b')' => encoded.push(*byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Posts one file and answers the status with the parsed reply.
async fn upload(handle: &GatewayHandle, name: &str, kind: &str, body: Vec<u8>) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/upload?name={}&kind={kind}",
            handle.addr,
            encode(name)
        ))
        .header("Authorization", "Bearer test-token")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

fn upload_dir(data_dir: &Path, id: &str) -> std::path::PathBuf {
    data_dir.join("uploads").join(id)
}

/// How many upload directories exist, so a test can say nothing was left behind.
fn stored_uploads(data_dir: &Path) -> usize {
    std::fs::read_dir(data_dir.join("uploads"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// The status line of a hand-written request. Reading it is how a raw test knows
/// the gateway answered without the body it did not send.
async fn status_of(stream: &mut tokio::net::TcpStream) -> u16 {
    use tokio::io::AsyncReadExt;
    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            stream.read(&mut buffer),
        )
        .await
        .expect("the gateway answered")
        .expect("the response head arrived");
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buffer[..read]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head);
    head.split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no status line in {head:?}"))
        .parse()
        .unwrap()
}

/// A valid one-pixel PNG: what the image pipeline accepts without re-encoding.
fn one_pixel_png() -> Vec<u8> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC")
        .unwrap()
}

/// A project and a task on the gateway, for the commands that need one.
async fn a_task(handle: &GatewayHandle, data_dir: &Path) -> (String, String) {
    let project = call(handle, "add_project", json!({ "path": data_dir })).await;
    let project_id = project["id"].as_str().unwrap().to_owned();
    let session = call(
        handle,
        "create_session",
        json!({ "projectId": project_id, "provider": "claude", "model": "sonnet" }),
    )
    .await;
    (project_id, session["id"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn upload_is_guarded_by_the_same_token_as_everything_else() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let url = format!("http://{}/upload?name=notes.txt&kind=document", handle.addr);
    for authorization in [None, Some("Bearer wrong-token")] {
        let mut request = reqwest::Client::new().post(&url).body("hello");
        if let Some(value) = authorization {
            request = request.header("Authorization", value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401);
        assert_eq!(response.text().await.unwrap(), "unauthorized");
    }
    assert_eq!(
        stored_uploads(dir.path()),
        0,
        "a refused request must not reach the handler and store anything"
    );

    let (status, body) = upload(&handle, "notes.txt", "document", b"hello".to_vec()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["name"], "notes.txt");
    assert_eq!(body["kind"], "document");
    assert_eq!(body["bytes"], 5);
    assert_eq!(
        body["sha256"],
        format!("{:x}", Sha256::digest(b"hello")),
        "the digest is computed as the bytes arrive"
    );
    assert!(body["createdAt"].is_i64());
    handle.stop().await;
}

#[tokio::test]
async fn a_client_name_never_becomes_a_path() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let bytes = b"not a real file, just bytes".to_vec();
    let long = "a".repeat(300);
    for name in [
        "../../etc/passwd",
        "/etc/passwd",
        "with\0nul",
        "trailing/",
        long.as_str(),
        "photo (1) — café.png",
    ] {
        let (status, body) = upload(&handle, name, "image", bytes.clone()).await;
        assert_eq!(status, 200, "{name}: {body}");
        let id = body["id"].as_str().unwrap();
        // The bytes are at <uuid>/data whatever the name said, and the name is only
        // in the metadata.
        assert_eq!(
            std::fs::read(upload_dir(dir.path(), id).join("data")).unwrap(),
            bytes
        );
        let meta: Value = serde_json::from_slice(
            &std::fs::read(upload_dir(dir.path(), id).join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["name"], name);
        assert_eq!(meta["kind"], "image");
        assert_eq!(body["name"], name);
        // Nothing escaped the uploads directory.
        assert!(!dir.path().join("etc").exists());
        assert!(!dir.path().join("passwd").exists());
    }
    handle.stop().await;
}

#[tokio::test]
async fn an_oversized_declared_length_is_refused_before_the_body() {
    use tokio::io::AsyncWriteExt;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let mut stream = tokio::net::TcpStream::connect(handle.addr).await.unwrap();
    // A gigabyte declared, and not one byte of it sent: the answer has to come from
    // the header alone.
    let head = "POST /upload?name=huge.bin&kind=document HTTP/1.1\r\nHost: localhost\r\n\
                Authorization: Bearer test-token\r\nContent-Length: 1000000000\r\n\r\n";
    stream.write_all(head.as_bytes()).await.unwrap();
    assert_eq!(status_of(&mut stream).await, 413);
    assert_eq!(stored_uploads(dir.path()), 0);
    handle.stop().await;
}

#[tokio::test]
async fn a_chunked_body_that_crosses_the_cap_is_aborted_and_leaves_nothing() {
    use tokio::io::AsyncWriteExt;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start_with_cap(dir.path(), 64 * 1024).await;
    let mut stream = tokio::net::TcpStream::connect(handle.addr).await.unwrap();
    // Chunked, so no declared length can be checked: only the running count can
    // stop this, and it has to stop it mid-transfer.
    let head = "POST /upload?name=stream.bin&kind=document HTTP/1.1\r\nHost: localhost\r\n\
                Authorization: Bearer test-token\r\nTransfer-Encoding: chunked\r\n\r\n";
    stream.write_all(head.as_bytes()).await.unwrap();
    let chunk = vec![b'x'; 8 * 1024];
    let frame = format!("{:x}\r\n", chunk.len());
    // Ten chunks is 80 KiB against a 64 KiB cap: the ninth crosses it. Nothing is
    // read between writes on purpose — the answer only comes once the count is
    // passed, so waiting for it earlier would wait forever.
    for _ in 0..10 {
        if stream.write_all(frame.as_bytes()).await.is_err()
            || stream.write_all(&chunk).await.is_err()
            || stream.write_all(b"\r\n").await.is_err()
        {
            // The gateway may already have answered and stopped reading.
            break;
        }
    }
    assert_eq!(status_of(&mut stream).await, 413);
    assert_eq!(
        stored_uploads(dir.path()),
        0,
        "an aborted transfer leaves no partial upload behind"
    );
    handle.stop().await;
}

#[tokio::test]
async fn the_reply_is_the_metadata_and_a_second_file_is_a_second_upload() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (first_status, first) = upload(&handle, "one.txt", "document", b"one".to_vec()).await;
    let (second_status, second) = upload(&handle, "one.txt", "document", b"two".to_vec()).await;
    assert_eq!((first_status, second_status), (200, 200));
    assert_ne!(
        first["id"], second["id"],
        "the same filename twice is two uploads, not one overwritten"
    );
    assert_eq!(stored_uploads(dir.path()), 2);
    handle.stop().await;
}

#[tokio::test]
async fn an_unknown_upload_id_fails_the_send_before_a_turn_starts() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (_project, session) = a_task(&handle, dir.path()).await;
    let missing = "3f2f4c0e-0f2f-4c40-9f2f-2a1f3b4c5d6e";
    let body = rpc(
        &handle,
        "send_message",
        json!({ "sessionId": session, "text": "here it is", "uploadIds": [missing] }),
    )
    .await;
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["error"], format!("unknown_upload: {missing}"));
    assert_eq!(
        call(&handle, "gateway_live_tasks", json!({})).await,
        json!([]),
        "a refused send is not a turn that started"
    );
    handle.stop().await;
}

#[tokio::test]
async fn the_sweep_keeps_what_a_queued_message_still_needs() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (_project, session) = a_task(&handle, dir.path()).await;
    let png = one_pixel_png();
    let (_, loose) = upload(&handle, "loose.png", "image", png.clone()).await;
    let (_, kept) = upload(&handle, "kept.png", "image", png).await;
    call(
        &handle,
        "queue_add",
        json!({
            "sessionId": session,
            "text": "after this run",
            "attachments": [{ "kind": "image", "id": kept["id"] }],
        }),
    )
    .await;

    // A day later: the queued message still needs its upload, the other one is
    // scratch space that nothing refers to.
    let (removed, retained) = handle.sweep_uploads(now_ms() + 25 * 60 * 60 * 1000).unwrap();
    assert_eq!((removed, retained), (1, 1), "one of each, decided by reference");
    assert!(!upload_dir(dir.path(), loose["id"].as_str().unwrap()).exists());
    assert!(upload_dir(dir.path(), kept["id"].as_str().unwrap()).exists());

    // Seven days is the hard cap: being referenced does not make it permanent.
    let (removed, retained) = handle.sweep_uploads(now_ms() + 8 * 24 * 60 * 60 * 1000).unwrap();
    assert_eq!((removed, retained), (1, 0));
    assert!(!upload_dir(dir.path(), kept["id"].as_str().unwrap()).exists());

    // And an id whose file is gone is refused before a turn, like any unknown one.
    let body = rpc(
        &handle,
        "send_message",
        json!({ "sessionId": session, "text": "too late", "uploadIds": [kept["id"]] }),
    )
    .await;
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(
        body["error"],
        format!("unknown_upload: {}", kept["id"].as_str().unwrap())
    );
    handle.stop().await;
}

#[tokio::test]
async fn a_gateway_sweeps_aged_uploads_before_it_serves() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (_, body) = upload(&handle, "forgotten.bin", "document", b"bytes".to_vec()).await;
    let id = body["id"].as_str().unwrap().to_owned();
    // Age it past the unreferenced day, then let a fresh gateway boot sweep it:
    // nothing refers to this upload and nothing ever did.
    let meta_path = upload_dir(dir.path(), &id).join("meta.json");
    let mut meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    meta["createdAt"] = json!(now_ms() - 25 * 60 * 60 * 1000);
    std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    handle.stop().await;
    assert!(upload_dir(dir.path(), &id).exists(), "still there before the reboot");

    let restarted = start(dir.path()).await;
    assert!(
        !upload_dir(dir.path(), &id).exists(),
        "the boot sweep removes what a day of nothing left behind"
    );
    assert_eq!(stored_uploads(dir.path()), 0);
    restarted.stop().await;
}

#[tokio::test]
async fn an_uploaded_pdf_is_read_on_the_send_path_instead_of_refused() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (_project, session) = a_task(&handle, dir.path()).await;

    // A one-page PDF with a correct xref table, so poppler reads it as a real file.
    let content = "0 0 0 rg 72 700 200 100 re f";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
    }
    let xref = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
    for offset in &offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));

    let (status, scanned) = upload(&handle, "scan.pdf", "document", pdf.into_bytes()).await;
    assert_eq!(status, 200, "{scanned}");

    // The page draws no glyph, so the extractor has to answer for that: this proves
    // the send path now runs the reader (task 8) rather than the old blanket refusal,
    // and that an empty result never looks like an empty document.
    let body = rpc(
        &handle,
        "send_message",
        json!({ "sessionId": session, "text": "read this", "uploadIds": [scanned["id"]] }),
    )
    .await;
    assert_eq!(body["ok"], false, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("No readable text"),
        "the extractor ran and reported honestly: {error}"
    );

    // And a PDF that *has* text is read all the way through: the send gets past the
    // document and is refused by the image cap instead, which is the order the
    // admission path runs in. A document failure would have answered first.
    let text = "BT /F1 18 Tf 72 700 Td (Lighthouse keeper duty) Tj ET";
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
        format!("<< /Length {} >>\nstream\n{text}\nendstream", text.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
    }
    let xref = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
    for offset in &offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    let (_, read) = upload(&handle, "brief.pdf", "document", pdf.into_bytes()).await;
    let screenshots: Vec<String> = (0..5).map(|_| uuid_like()).collect();
    let body = rpc(
        &handle,
        "send_message",
        json!({
            "sessionId": session,
            "text": "read this",
            "imageIds": screenshots,
            "uploadIds": [read["id"]],
        }),
    )
    .await;
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(
        body["error"],
        "Attach up to four screenshots per message.",
        "the PDF resolved and the image cap answered, in that order"
    );
    assert_eq!(
        call(&handle, "gateway_live_tasks", json!({})).await,
        json!([]),
        "no turn started"
    );
    handle.stop().await;
}

/// A well-formed uuid the gateway has never stored, for a capture id that only has
/// to count.
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let value = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("00000000-0000-4000-8000-{value:012}")
}

/// The proof that a cap-sized upload is streamed rather than buffered: the real
/// `pandacode-gateway` binary takes 200 MiB in one request while its own resident
/// memory is sampled from `/proc`.
///
/// A separate process on purpose: an in-process server would make the number a
/// statement about this test binary. The client streams 1 MiB chunks and never
/// holds the file either, so growth in the gateway can only be its own doing.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_hundred_mebibyte_upload_does_not_become_two_hundred_mebibytes_of_memory() {
    use std::io::BufRead;
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pandacode-gateway"))
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("0")
        .arg("--token")
        .arg("test-token")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let addr: std::net::SocketAddr = line
        .trim()
        .rsplit("http://")
        .next()
        .expect("the gateway printed its address")
        .parse()
        .expect("the printed address parsed");

    fn resident_kb(pid: u32) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .expect("VmRSS is in /proc/<pid>/status")
    }

    let pid = child.id();
    let baseline = resident_kb(pid);
    let total = 200 * 1024 * 1024;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /upload?name=big.bin&kind=document HTTP/1.1\r\nHost: localhost\r\n\
         Authorization: Bearer test-token\r\nContent-Length: {total}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let chunk = vec![b'x'; 1024 * 1024];
    let mut peak = baseline;
    for _ in 0..200 {
        stream.write_all(&chunk).await.unwrap();
        peak = peak.max(resident_kb(pid));
    }
    assert_eq!(status_of(&mut stream).await, 200);
    let data = std::fs::read_dir(dir.path().join("uploads"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("data"))
        .find(|path| path.exists())
        .expect("the upload is on disk");
    assert_eq!(
        std::fs::metadata(data).unwrap().len(),
        total as u64,
        "all 200 MiB arrived"
    );

    // What the mechanism costs: the peak of the gateway's own RSS against its
    // baseline. Buffering the body would be ~200 MiB of growth; a stream is a lot
    // less than the file.
    let growth_mb = (peak.saturating_sub(baseline)) as f64 / 1024.0;
    eprintln!(
        "gateway RSS: baseline {:.1} MB, peak {:.1} MB, growth {:.1} MB for a {} MB upload",
        baseline as f64 / 1024.0,
        peak as f64 / 1024.0,
        growth_mb,
        total / (1024 * 1024)
    );
    assert!(
        growth_mb < 32.0,
        "a {total} byte upload grew the gateway by {growth_mb:.1} MB"
    );
    let _ = child.kill();
    let _ = child.wait();
}

// ---- terminals over the socket (M2 task 9, PAN-110) ------------------------
//
// A shell is a real PTY in this process, so these tests type into a real shell and
// read what comes back off the socket. The shell's cwd is the temp data directory,
// so nothing escapes into the owner's session.
//
// Two habits make these honest: a socket is subscribed **before** the command is
// typed (a fresh socket without `since` sees only what happens after it connects),
// and every expected string is a value the shell computes, never the text of the
// command — the PTY echoes what was typed, so a marker written literally would
// satisfy a naive check before the shell had run anything.

/// One terminal test at a time. They spawn real shells and one of them measures this
/// process's own descriptors, so running them beside each other would measure the
/// neighbours as much as the test.
async fn terminal_guard() -> tokio::sync::OwnedSemaphorePermit {
    static SLOTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let slots = SLOTS.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)));
    slots
        .clone()
        .acquire_owned()
        .await
        .expect("the terminal slot is never closed")
}

/// A project and a shell in it, over the gateway.
async fn start_shell(handle: &GatewayHandle, dir: &Path) -> (String, String) {
    let (project, _session) = a_task(handle, dir).await;
    let id = call(
        handle,
        "start_terminal",
        json!({ "projectId": project, "cols": 100, "rows": 30 }),
    )
    .await;
    (project, id.as_str().unwrap().to_owned())
}

/// Every frame on this socket until `done` says stop. A shell's own startup reads
/// the machine's profile, so a loaded host can take a while before the first byte.
async fn frames_until(socket: &mut Socket, mut done: impl FnMut(&[Value]) -> bool) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(90), socket.next())
            .await
            .expect("a terminal frame arrived")
            .expect("the terminal socket stayed open")
            .expect("the frame was a message");
        frames.push(serde_json::from_str(frame.to_text().unwrap()).unwrap());
        if done(&frames) {
            return frames;
        }
    }
}

/// The terminal output in these frames, concatenated.
fn terminal_text(frames: &[Value]) -> String {
    frames
        .iter()
        .filter(|frame| frame["event"] == "terminal-event")
        .filter(|frame| frame["payload"]["exit"] == false)
        .filter_map(|frame| frame["payload"]["data"].as_str())
        .collect::<String>()
}

fn terminal_seqs(frames: &[Value]) -> Vec<u64> {
    frames.iter().filter_map(|frame| frame["seq"].as_u64()).collect()
}

/// Waits until the terminal's scrollback contains `marker`, by asking for the replay
/// again each time. A live socket can be outrun by a flood (the subscriber queue
/// lags and the frame that carried the marker is dropped), while a fresh socket
/// always gets what the ring still holds.
async fn wait_for_replay(handle: &GatewayHandle, id: &str, marker: &str) -> Vec<Value> {
    for _ in 0..60 {
        let mut socket = events_socket(handle.addr, &format!("&terminal={id}&since=0")).await;
        let mut frames = Vec::new();
        // The ring is drained for now: the shell may still be printing.
        while let Ok(Some(Ok(message))) =
            tokio::time::timeout(std::time::Duration::from_secs(5), socket.next()).await
        {
            frames.push(serde_json::from_str(message.to_text().unwrap()).unwrap());
            if terminal_text(&frames).contains(marker) {
                return frames;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    panic!("the shell never printed {marker:?} into its ring");
}

/// Types one line into the shell and returns everything printed until `expect`
/// shows up. `expect` must be a value the shell computes.
async fn run_command(
    handle: &GatewayHandle,
    socket: &mut Socket,
    id: &str,
    command: &str,
    expect: &str,
) -> String {
    call(
        handle,
        "write_terminal",
        json!({ "terminalId": id, "data": format!("{command}\n") }),
    )
    .await;
    let frames = frames_until(socket, |frames| terminal_text(frames).contains(expect)).await;
    terminal_text(&frames)
}

#[cfg(target_os = "linux")]
fn running(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// A child of `parent`, retried for a moment because a fork is not instantaneous.
#[cfg(target_os = "linux")]
fn child_of(parent: u32) -> u32 {
    for _ in 0..40 {
        if let Ok(output) = std::process::Command::new("ps")
            .args(["-o", "pid=", "--ppid", &parent.to_string()])
            .output()
        {
            if let Some(pid) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.trim().parse::<u32>().ok())
            {
                return pid;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("no child process for {parent}");
}

#[tokio::test]
async fn a_reconnect_replays_the_tail_of_the_scrollback_in_order() {
    let _serial = terminal_guard().await;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (_project, id) = start_shell(&handle, dir.path()).await;

    // A non-repeating sequence, long enough to be many PTY reads. The last number
    // is multiplied so the echo of the command cannot satisfy the wait.
    call(
        &handle,
        "write_terminal",
        json!({
            "terminalId": id,
            "data": "i=1; while [ $i -le 400 ]; do echo \"line $i\"; i=$((i+1)); done; echo \"tail $((200*2))\"\n"
        }),
    )
    .await;

    // Reconnect from the beginning: the ring's tail, in order, no duplicates.
    let frames = wait_for_replay(&handle, &id, "tail 400").await;
    let text = terminal_text(&frames);
    let mut last = 0u32;
    let mut seen = 0;
    for line in text.lines() {
        let Some(number) = line
            .trim()
            .strip_prefix("line ")
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        assert!(
            last == 0 || number == last + 1,
            "the replay reordered or skipped: {number} after {last}"
        );
        last = number;
        seen += 1;
    }
    assert!(seen > 1, "the replay carried the scrollback: {text:?}");
    assert_eq!(last, 400, "the tail ends where the shell stopped");
    let seqs = terminal_seqs(&frames);
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "frames arrive in one increasing order: {seqs:?}"
    );

    // And a resume from a number inside the ring gives exactly what follows it.
    let middle = seqs[seqs.len() / 2];
    let mut resumed = events_socket(handle.addr, &format!("&terminal={id}&since={middle}")).await;
    let resumed_frames = frames_until(&mut resumed, |frames| {
        terminal_text(frames).contains("tail 400")
    })
    .await;
    let resumed_seqs = terminal_seqs(&resumed_frames);
    assert_eq!(
        resumed_seqs.first().copied(),
        Some(middle + 1),
        "a resume starts at the frame after the one it named: {resumed_seqs:?}"
    );
    assert!(
        resumed_seqs.len() < seqs.len(),
        "a resume is shorter than a full replay"
    );
    handle.stop().await;
}

#[tokio::test]
async fn a_reconnect_from_before_the_ring_is_told_to_reset() {
    let _serial = terminal_guard().await;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (_project, id) = start_shell(&handle, dir.path()).await;

    // Enough output to push the 256 KiB ring past its start, several times over.
    call(
        &handle,
        "write_terminal",
        json!({
            "terminalId": id,
            "data": "i=1; while [ $i -le 5000 ]; do echo \"chatter $i aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"; i=$((i+1)); done; echo \"end $((200*2))\"\n"
        }),
    )
    .await;
    // The flood has to have reached the ring before the resume can be judged: the
    // ring holding the tail is what makes `since=1` older than it.
    let _ = wait_for_replay(&handle, &id, "end 400").await;

    // `since=1` is older than the ring holds: the client is told to reset, not
    // handed a stream that quietly starts in the middle.
    let mut late = events_socket(handle.addr, &format!("&terminal={id}&since=1")).await;
    let first = next_frame(&mut late).await;
    assert_eq!(
        first["event"], "terminal-reset",
        "a terminal resume past the ring is announced, not silently skipped: {first}"
    );
    assert_eq!(first["payload"]["fromSeq"], 1);
    let lost_through = first["payload"]["toSeq"].as_u64().unwrap();
    assert!(lost_through > 1, "the frame names what was lost: {first}");

    // What follows is the ring's own content, in order, from after the loss.
    let after = frames_until(&mut late, |frames| frames.len() > 5).await;
    let seqs = terminal_seqs(&after);
    assert!(
        seqs.iter().all(|seq| *seq > lost_through),
        "the redraw starts after the announced loss: {seqs:?} vs {lost_through}"
    );
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "the redraw is ordered: {seqs:?}"
    );
    handle.stop().await;
}

#[tokio::test]
async fn every_frame_is_valid_utf8_and_the_stream_equals_what_was_written() {
    let _serial = terminal_guard().await;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (_project, id) = start_shell(&handle, dir.path()).await;

    // A 3-byte character, thousands of times over: PTY reads land mid-character
    // unless the split is carried between reads.
    let mut socket = events_socket(handle.addr, &format!("&terminal={id}")).await;
    call(
        &handle,
        "write_terminal",
        json!({
            "terminalId": id,
            "data": "printf '\\342\\202\\254%.0s' $(seq 1 3000); echo; echo \"euros $((20*2)) done\"\n"
        }),
    )
    .await;
    let frames = frames_until(&mut socket, |frames| {
        terminal_text(frames).contains("euros 40 done")
    })
    .await;
    let text = terminal_text(&frames);
    assert!(
        !text.contains('\u{FFFD}'),
        "a split codepoint became a replacement character"
    );
    assert_eq!(
        text.matches('€').count(),
        3000,
        "every euro sign survived exactly once: {}",
        text.matches('€').count()
    );
    for frame in &frames {
        let data = frame["payload"]["data"].as_str().unwrap_or_default();
        assert!(!data.contains('\u{FFFD}'), "no frame carries a replacement character");
    }
    handle.stop().await;
}

#[tokio::test]
async fn two_terminals_never_cross_on_their_sockets() {
    let _serial = terminal_guard().await;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (project, first) = start_shell(&handle, dir.path()).await;
    let second = call(
        &handle,
        "start_terminal",
        json!({ "projectId": project, "cols": 100, "rows": 30 }),
    )
    .await;
    let second = second.as_str().unwrap().to_owned();

    let mut one = events_socket(handle.addr, &format!("&terminal={first}")).await;
    let mut two = events_socket(handle.addr, &format!("&terminal={second}")).await;
    // Each shell answers on its own before the cross-check, so a slow start cannot
    // be mistaken for a crossed filter.
    let warmed = run_command(&handle, &mut one, &first, "echo \"warm $((20*2))\"", "warm 40").await;
    assert!(warmed.contains("warm 40"), "the first shell answers: {warmed:?}");
    let warmed = run_command(&handle, &mut two, &second, "echo \"warm $((20*2))\"", "warm 40").await;
    assert!(warmed.contains("warm 40"), "the second shell answers: {warmed:?}");
    let first_text = run_command(&handle, &mut one, &first, "echo \"first $((20*2))\"", "first 40").await;
    let second_text =
        run_command(&handle, &mut two, &second, "echo \"second $((20*2))\"", "second 40").await;
    assert!(first_text.contains("first 40"), "{first_text:?}");
    assert!(second_text.contains("second 40"), "{second_text:?}");
    assert!(!first_text.contains("second 40"), "the first shell saw the second's output");
    assert!(!second_text.contains("first 40"), "the second shell saw the first's output");
    handle.stop().await;
}

#[tokio::test]
async fn a_resize_survives_a_reconnect_and_still_clamps() {
    let _serial = terminal_guard().await;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (_project, id) = start_shell(&handle, dir.path()).await;

    call(&handle, "resize_terminal", json!({ "terminalId": id, "cols": 120, "rows": 40 })).await;
    // A reconnect is a new socket on the same terminal, which is what a client does
    // after a transport change.
    drop(events_socket(handle.addr, &format!("&terminal={id}")).await);
    let mut socket = events_socket(handle.addr, &format!("&terminal={id}")).await;
    let text = run_command(&handle, &mut socket, &id, "stty size", "40 120").await;
    assert!(text.contains("40 120"), "the PTY kept its size across the reconnect: {text:?}");

    // The clamp still applies at the extremes.
    call(&handle, "resize_terminal", json!({ "terminalId": id, "cols": 1, "rows": 1 })).await;
    let text = run_command(&handle, &mut socket, &id, "stty size", "2 10").await;
    assert!(text.contains("2 10"), "the clamp is still the shell's real size: {text:?}");
    handle.stop().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_idle_terminal_is_closed_with_its_process_group_and_told_why() {
    let _serial = terminal_guard().await;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    // A one-second window keeps the test short; the tick is a quarter of it.
    let mut config = test_config(dir.path(), "127.0.0.1:0");
    config.terminal_idle = std::time::Duration::from_secs(1);
    let handle = spawn(config).await.unwrap();
    let (_project, id) = start_shell(&handle, dir.path()).await;

    // The shell's own pid, and one job inside it, so "the group is gone" is not just
    // "the first process exited".
    let activity = call(&handle, "get_terminal_activity", json!({ "terminalId": id })).await;
    let shell = activity["pid"].as_u64().expect("the shell has a pid") as u32;
    call(&handle, "write_terminal", json!({ "terminalId": id, "data": "sleep 300&\n" })).await;
    let child = child_of(shell);
    assert!(running(shell), "the shell is alive before the close");
    assert!(running(child), "the job is alive before the close");

    // Nobody attached and nothing printed since: the rule closes it after the
    // window, and a client that comes back is told why rather than finding a shell
    // that silently vanished.
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    let mut after = events_socket(handle.addr, &format!("&terminal={id}&since=0")).await;
    let frames = frames_until(&mut after, |frames| {
        frames.iter().any(|frame| frame["payload"]["exit"] == true)
    })
    .await;
    let exit = frames
        .iter()
        .find(|frame| frame["payload"]["exit"] == true)
        .expect("an exit frame");
    assert_eq!(exit["payload"]["reason"], "idle", "the close says why: {exit}");
    assert!(
        !running(shell) && !running(child),
        "the whole process group is gone, not just the first process: shell {shell}, child {child}"
    );
    handle.stop().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn output_or_a_watching_client_keeps_an_idle_terminal_alive() {
    let _serial = terminal_guard().await;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(dir.path(), "127.0.0.1:0");
    config.terminal_idle = std::time::Duration::from_secs(1);
    let handle = spawn(config).await.unwrap();
    let (_project, id) = start_shell(&handle, dir.path()).await;

    // A client attached and silent: past the window, the shell still runs.
    let mut watching = events_socket(handle.addr, &format!("&terminal={id}")).await;
    tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
    let text = run_command(&handle, &mut watching, &id, "echo \"alive $((20*2))\"", "alive 40").await;
    assert!(
        text.contains("alive 40"),
        "a watched shell was closed while a client was attached: {text:?}"
    );

    // Detached but printing: also alive, past the window.
    drop(watching);
    let mut detached = events_socket(handle.addr, &format!("&terminal={id}")).await;
    for step in 0..3 {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let text = run_command(
            &handle,
            &mut detached,
            &id,
            "echo \"tick $((20*2))\"",
            "tick 40",
        )
        .await;
        assert!(
            text.contains("tick 40"),
            "output alone keeps a shell alive (step {step}): {text:?}"
        );
    }
    handle.stop().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn opening_and_closing_terminals_returns_the_descriptor_count_to_baseline() {
    let _serial = terminal_guard().await;
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let (project, _session) = a_task(&handle, dir.path()).await;

    let open_fds = || {
        std::fs::read_dir("/proc/self/fd")
            .map(|entries| entries.count())
            .unwrap_or(0)
    };
    // One warm start, so the baseline includes whatever the first terminal allocates.
    let (_project, warm) = start_shell(&handle, dir.path()).await;
    call(&handle, "close_terminal", json!({ "terminalId": warm })).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let baseline = open_fds();

    for _ in 0..8 {
        let id = call(
            &handle,
            "start_terminal",
            json!({ "projectId": project, "cols": 80, "rows": 24 }),
        )
        .await;
        let id = id.as_str().unwrap().to_owned();
        // A resize storm on each, the classic way this type leaks.
        for cols in [80u16, 100, 60, 120] {
            call(
                &handle,
                "resize_terminal",
                json!({ "terminalId": id, "cols": cols, "rows": 24 }),
            )
            .await;
        }
        call(&handle, "close_terminal", json!({ "terminalId": id })).await;
    }
    // Give the reader threads their moment to see the close and drop the PTY.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    let after = open_fds();
    assert!(
        after <= baseline + 2,
        "terminals leaked descriptors: {baseline} before, {after} after (8 opened and closed)"
    );
    handle.stop().await;
}

// ---- PAN-112: the audit trail, the auth limit, and the unit as the owner ----------

/// The directives systemd would read from the shipped unit: `key=value` pairs, with
/// comments and blank lines dropped. A test that asserted on the raw text would pass
/// on a comment that merely mentions a directive.
fn unit_directives() -> Vec<(String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../scripts/gateway/pandacode-gateway.service");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('['))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

fn unit_value(key: &str) -> Option<String> {
    unit_directives()
        .into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

#[test]
fn the_unit_runs_as_the_owner_and_says_why_isolation_is_gone() {
    // The M2 decision (spec §2.2): the gateway IS the person. An ephemeral user has
    // no credentials, no git config, no provider CLIs and cannot read a project.
    assert_eq!(
        unit_value("User").as_deref(),
        Some("abacus"),
        "the unit must run as the owner"
    );
    assert!(
        unit_value("Group").is_some(),
        "and name the group rather than inheriting an unpredictable one"
    );
    // The ephemeral-user machinery is what removed the identity; both go together.
    for gone in [
        "DynamicUser",
        "StateDirectory",
        "StateDirectoryMode",
        "ReadWritePaths",
        "ProtectSystem",
        "PrivateTmp",
        "ProtectHome",
    ] {
        assert_eq!(
            unit_value(gone),
            None,
            "{gone}= must not be in the unit: it is either the M1 ephemeral-user shape \
             or a sandbox this decision removed"
        );
    }
    // Kept on purpose.
    assert_eq!(unit_value("NoNewPrivileges").as_deref(), Some("yes"));
    assert_eq!(unit_value("UMask").as_deref(), Some("0077"));
    // The audit's destination: without this the lines the gateway prints are lost.
    assert_eq!(unit_value("StandardOutput").as_deref(), Some("journal"));

    let exec = unit_value("ExecStart").expect("ExecStart is required");
    assert!(
        !exec.contains("--data-dir"),
        "the data directory must be the owner's default, not a system one: {exec}"
    );
    assert!(
        exec.contains("pandacode-gateway") && exec.contains("--port"),
        "the unit starts the gateway on its loopback port: {exec}"
    );

    // The reason has to be written down where the next person edits the unit, or
    // somebody re-tightens ProtectSystem "for safety" and breaks every build.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../scripts/gateway/pandacode-gateway.service");
    let text = std::fs::read_to_string(path).unwrap();
    assert!(
        text.contains("GONE BY DECISION"),
        "a comment must say the isolation was removed deliberately"
    );
    assert!(
        text.contains("~/.local/share/ai.pandacode.desktop/gateway-token"),
        "and where the token is, since it is no longer under /var/lib"
    );
}

#[tokio::test]
async fn every_command_leaves_one_audit_line_and_no_line_carries_the_work() {
    install_crypto_provider();
    // Unique strings, so the assertions can pick their own lines out of a process
    // that other tests are writing to at the same time.
    let canary = "AUDIT-CANARY-message-text-must-not-be-logged-7f31";
    let file_name = "AUDIT-NAME-canary-8c2a.txt";
    let file_body = "AUDIT-CONTENT-canary-9d4b";
    let recording = pandacode_lib::gateway::audit::record();

    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let project = call(&handle, "add_project", json!({ "path": dir.path() })).await;
    let session = call(
        &handle,
        "create_session",
        json!({ "projectId": project["id"], "provider": "claude", "model": "sonnet" }),
    )
    .await;
    let session_id = session["id"].as_str().unwrap().to_owned();

    // 1. A command that carries text: queued, so the text really is stored.
    call(
        &handle,
        "queue_add",
        json!({ "sessionId": session_id, "text": canary }),
    )
    .await;

    // 2. An approval decision. Nothing is running, so the answer is refused — and
    //    the decision the client made is still what the journal has to show.
    let request_id = "audit-request-4e1f";
    let refused = rpc(
        &handle,
        "respond_approval",
        json!({ "sessionId": session_id, "requestId": request_id, "approved": true }),
    )
    .await;
    assert_eq!(refused["ok"], false, "no run is waiting for this answer");

    // 3. An upload, which is an event with its own fields.
    let (status, receipt) = upload(&handle, file_name, "document", file_body.as_bytes().to_vec()).await;
    assert_eq!(status, 200, "the upload was accepted: {receipt}");
    let upload_id = receipt["id"].as_str().unwrap().to_owned();

    let lines = recording.lines();

    // One line per command, naming what was asked and whether it answered.
    let command = lines
        .iter()
        .find(|line| line.contains("event=command") && line.contains("command=queue_add") && line.contains(&format!("session={session_id}")))
        .unwrap_or_else(|| panic!("no command line for queue_add in {lines:#?}"));
    assert!(command.contains("outcome=ok"), "{command}");
    assert!(command.contains("client="), "the source is on the line: {command}");
    assert!(command.contains("duration_ms="), "{command}");

    let approval = lines
        .iter()
        .find(|line| line.contains("event=approval") && line.contains(&format!("request={request_id}")))
        .unwrap_or_else(|| panic!("no approval line in {lines:#?}"));
    assert!(approval.contains("decision=approved"), "{approval}");
    assert!(approval.contains(&format!("session={session_id}")), "{approval}");

    let uploaded = lines
        .iter()
        .find(|line| line.contains("event=upload") && line.contains(&format!("upload={upload_id}")))
        .unwrap_or_else(|| panic!("no upload line in {lines:#?}"));
    assert!(uploaded.contains("kind=document"), "{uploaded}");
    assert!(
        uploaded.contains(&format!("bytes={}", file_body.len())),
        "{uploaded}"
    );
    // The digest on the line is the one the client was given, so the journal can be
    // matched to the file later.
    assert!(
        uploaded.contains(&format!("sha256={}", receipt["sha256"].as_str().unwrap())),
        "{uploaded}"
    );

    // The rule the whole module exists for: nothing the person was working on is in
    // the journal. Not the queued text, not the filename, not the file's bytes.
    for line in &lines {
        assert!(!line.contains(canary), "message text reached the journal: {line}");
        assert!(!line.contains(file_name), "a filename reached the journal: {line}");
        assert!(!line.contains(file_body), "file content reached the journal: {line}");
    }

    handle.stop().await;
}

#[tokio::test]
async fn eleven_bad_tokens_from_one_address_are_limited_while_the_owner_is_not() {
    install_crypto_provider();
    let recording = pandacode_lib::gateway::audit::record();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let base = format!("http://{}", handle.addr);
    let request = json!({ "id": "1", "command": "list_projects", "args": {} });
    let client = reqwest::Client::new();

    // Ten wrong tokens are answered "unauthorized"; the eleventh is over the limit.
    let mut statuses = Vec::new();
    for _ in 0..11 {
        statuses.push(
            client
                .post(format!("{base}/rpc"))
                .header("Authorization", "Bearer wrong-token")
                .json(&request)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
        );
    }
    assert_eq!(
        &statuses[..10],
        &[401u16; 10],
        "the first ten are ordinary auth failures: {statuses:?}"
    );
    assert_eq!(statuses[10], 429, "eleventh: {statuses:?}");

    // The limit is in the audit, or it is not a limit anybody can see.
    let limited = recording
        .lines()
        .into_iter()
        .find(|line| line.contains("event=auth") && line.contains("status=429"))
        .unwrap_or_else(|| panic!("no 429 audit line: {:#?}", recording.lines()));
    assert!(limited.contains("failures=11"), "{limited}");
    assert!(limited.contains("window_s=60"), "{limited}");
    assert!(limited.contains("path=/rpc"), "{limited}");

    // The brake must never catch the owner: the right token still works from the
    // address that is over the limit.
    let allowed = call(&handle, "list_projects", json!({})).await;
    assert_eq!(allowed, json!([]), "a valid token is never limited");

    // And one address's failures are not another's. 127.0.0.2 is a real source on
    // this host's loopback, so this is a second client, not a second header.
    let other = reqwest::Client::builder()
        .local_address("127.0.0.2".parse::<std::net::IpAddr>().unwrap())
        .build()
        .unwrap();
    let unaffected = other
        .post(format!("{base}/rpc"))
        .header("Authorization", "Bearer wrong-token")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(
        unaffected.status(),
        401,
        "a different source has its own window and is not limited by this one"
    );

    handle.stop().await;
}
