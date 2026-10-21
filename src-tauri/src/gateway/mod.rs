//! A standalone HTTP gateway: the desktop app's backend without Tauri.
//!
//! Compiled only under `--features gateway`, so the desktop build keeps its
//! dependency set and never starts a second server.

pub mod config;
pub mod server;

use std::net::SocketAddr;
use std::path::PathBuf;

/// Everything the gateway needs before it can serve a request.
pub struct GatewayConfig {
    /// Directory holding `kingcode.sqlite3`, the token and the lock file.
    pub data_dir: PathBuf,
    /// Address to listen on; `config::from_args` keeps it on loopback by default.
    pub bind: SocketAddr,
    /// Bearer token every request except the SPA's own files must present.
    pub token: String,
    /// Built frontend to serve; `None` leaves the gateway API-only.
    pub static_dir: Option<PathBuf>,
    /// Browser origins allowed to call the API; the app's webview by default.
    pub allowed_origins: Vec<String>,
}

/// A running gateway. `stop` is the only way to shut it down cleanly.
pub struct GatewayHandle {
    pub addr: SocketAddr,
    pub shutdown: tokio::sync::oneshot::Sender<()>,
    pub join: tokio::task::JoinHandle<()>,
}

impl GatewayHandle {
    /// Signals the server and waits for it, so a test never leaves a port behind.
    pub async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.join.await;
    }
}

/// Binds the listener and serves until the handle is stopped.
pub async fn spawn(config: GatewayConfig) -> std::io::Result<GatewayHandle> {
    let router = server::router(server::RouterState {
        token: config.token.clone(),
        allowed_origins: config.allowed_origins.clone(),
    });
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let addr = listener.local_addr()?;
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let served = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
        if let Err(error) = served {
            eprintln!("gateway stopped: {error}");
        }
    });
    Ok(GatewayHandle {
        addr,
        shutdown,
        join,
    })
}
