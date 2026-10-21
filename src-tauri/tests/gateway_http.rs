//! End-to-end HTTP tests against a real gateway on a temporary data directory.
//!
//! Gated on the `gateway` feature so a build without it compiles exactly what it
//! compiled before (`cargo test --lib`, `cargo clippy --all-targets`).
#![cfg(feature = "gateway")]

use std::path::Path;

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
async fn a_second_gateway_on_the_same_data_directory_is_refused() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let handle = start(dir.path()).await;
    let error = refusal(test_config(dir.path(), "127.0.0.1:0")).await;
    assert!(error.contains("another gateway is already running"), "{error}");
    handle.stop().await;

    // A clean stop releases the lock, so the next start needs no --force-owner.
    let again = start(dir.path()).await;
    again.stop().await;
}

#[tokio::test]
async fn a_lock_left_by_a_dead_process_is_reclaimed() {
    install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    let mut child = child;
    child.wait().unwrap();
    std::fs::write(dir.path().join("gateway.lock"), format!("{dead}\n")).unwrap();

    let handle = start(dir.path()).await;
    let listed = call(&handle, "list_projects", json!({})).await;
    assert!(listed.as_array().unwrap().is_empty());
    handle.stop().await;
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
