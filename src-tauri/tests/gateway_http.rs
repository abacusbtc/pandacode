//! End-to-end HTTP tests against a real gateway on a temporary data directory.
//!
//! Gated on the `gateway` feature so a build without it compiles exactly what it
//! compiled before (`cargo test --lib`, `cargo clippy --all-targets`).
#![cfg(feature = "gateway")]

use std::path::Path;

use pandacode_lib::gateway::{config, spawn, GatewayConfig};

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
    }
}

async fn start(data_dir: &Path) -> pandacode_lib::gateway::GatewayHandle {
    spawn(test_config(data_dir, "127.0.0.1:0")).await.unwrap()
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
    let rpc = serde_json::json!({ "id": "1", "command": "list_projects", "args": {} });

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
    // The placeholder the dispatcher replaces in the next task: the point here
    // is that the token carried the call past auth.
    assert_eq!(allowed.status(), 501);
    assert_eq!(allowed.text().await.unwrap(), "rpc is not implemented yet");
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
