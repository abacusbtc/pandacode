//! End-to-end HTTP tests against a real gateway on a temporary data directory.
//!
//! Gated on the `gateway` feature so a build without it compiles exactly what it
//! compiled before (`cargo test --lib`, `cargo clippy --all-targets`).
#![cfg(feature = "gateway")]

use pandacode_lib::gateway::{spawn, GatewayConfig};

#[tokio::test]
async fn healthz_returns_ok() {
    let dir = tempfile::tempdir().unwrap();
    let config = GatewayConfig {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().unwrap(),
        token: "test-token".into(),
        static_dir: None,
    };
    let handle = spawn(config).await.unwrap();
    let url = format!("http://{}/healthz", handle.addr);
    let response = reqwest::get(url).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    handle.stop().await;
}
