//! End-to-end HTTP tests against a real gateway on a temporary data directory.
//!
//! Gated on the `gateway` feature so a build without it compiles exactly what it
//! compiled before (`cargo test --lib`, `cargo clippy --all-targets`).
#![cfg(feature = "gateway")]

use std::path::Path;

use futures_util::StreamExt;
use pandacode_lib::gateway::{config, spawn, GatewayHandle, GatewayConfig};
use serde_json::{json, Value};

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
        force_owner: false,
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
