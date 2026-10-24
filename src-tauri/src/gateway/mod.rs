//! A standalone HTTP gateway: the desktop app's backend without Tauri.
//!
//! Compiled only under `--features gateway`, so the desktop build keeps its
//! dependency set and never starts a second server.

pub mod config;

mod commands;
mod approvals;
mod dispatch;
mod events;
mod live;
mod rpc;
mod server;
mod static_files;
mod terminals;

#[cfg(test)]
mod recovery_tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

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
    /// The largest single file `/upload` accepts.
    pub upload_max: usize,
    /// How long a terminal with no client and no output is kept (`--terminal-idle`);
    /// zero means never close one.
    pub terminal_idle: std::time::Duration,
    /// Take a lock held by another process, for a gateway that died uncleanly.
    pub force_owner: bool,
    /// How long an approval nobody answers stays pending. `None` disables
    /// auto-deny (`--approval-ttl 0`), which the gateway logs once at start.
    pub approval_ttl: Option<std::time::Duration>,
}

/// A running gateway. `stop` is the only way to shut it down cleanly.
pub struct GatewayHandle {
    pub addr: SocketAddr,
    pub shutdown: tokio::sync::oneshot::Sender<()>,
    pub join: tokio::task::JoinHandle<()>,
    /// This gateway's own sink, so a test never depends on which process-wide sink
    /// won a race.
    sink: Arc<events::BroadcastSink>,
    /// Auto-deny, when it is on. Aborted with the server so a stopped gateway
    /// cannot deny anything.
    watchdog: Option<tokio::task::JoinHandle<()>>,
    /// The hourly upload sweeper, stopped with the server.
    sweeper: tokio::task::JoinHandle<()>,
    /// The idle-terminal sweeper, when the rule is on.
    terminal_sweeper: Option<tokio::task::JoinHandle<()>>,
    /// What a sweep needs, kept here so the pass can also be asked for by a test.
    data_dir: PathBuf,
    state: Arc<crate::AppState>,
}

impl GatewayHandle {
    /// Signals the server and waits for it, so a test never leaves a port behind.
    pub async fn stop(self) {
        if let Some(watchdog) = &self.watchdog {
            watchdog.abort();
        }
        self.sweeper.abort();
        // A gateway going away does not leave the owner's shells running: the map
        // dropping each Terminal kills its process group (PAN-110).
        if let Ok(mut terminals) = self.state.terminals.lock() {
            terminals.clear();
        }
        if let Some(sweeper) = self.terminal_sweeper {
            sweeper.abort();
        }
        let _ = self.shutdown.send(());
        let _ = self.join.await;
    }

    /// Emits as the server would, straight into this gateway's `/events` sockets.
    /// Broadcasting is synchronous, so there is nothing to await; the plan's
    /// sketch made this async and that was ceremony.
    pub fn emit<T: serde::Serialize>(&self, name: &str, payload: T) {
        self.sink
            .publish(name, serde_json::to_value(payload).unwrap_or(serde_json::Value::Null));
    }

    /// Runs the upload sweep now, as the boot and hourly passes do, with `now` in
    /// milliseconds since the epoch. Production only ever passes the clock; a test
    /// passes a later one so it can age an upload instead of waiting a day for it.
    /// Answers `(removed, kept)`.
    pub fn sweep_uploads(&self, now: i64) -> Result<(usize, usize), String> {
        let root = crate::uploads::root(&self.data_dir);
        let store = self.state.store.lock().map_err(|error| error.to_string())?;
        let report = crate::uploads::sweep(&root, &store, now)?;
        Ok((report.removed, report.kept))
    }
}

/// Binds the listener and serves until the handle is stopped.
pub async fn spawn(config: GatewayConfig) -> std::io::Result<GatewayHandle> {
    // One writer per data directory: the desktop app claims this too, so starting
    // either while the other runs is a refusal rather than two silent SQLite
    // writers. On unix the kernel releases the lock however the holder dies.
    let lock = match config::claim(&config.data_dir, config.force_owner) {
        config::Claim::Held(lock) => lock,
        config::Claim::Busy(message) | config::Claim::Unavailable(message) => {
            return Err(std::io::Error::other(message))
        }
    };
    let state = crate::gateway_state(&config.data_dir).map_err(std::io::Error::other)?;
    let sink = events::BroadcastSink::new();
    // First gateway in this process owns the sink. A second one is a second port on
    // the same data directory, which the lock already refuses, so this is only a
    // guard against two sinks in one process.
    let _ = crate::events::set_sink(sink.clone());
    // A turn runs with the gateway's own context: the sink installed just above,
    // directories under the data directory this process owns, and its own surface,
    // which keeps the live registry true. Built after the sink, because a context
    // without one is refused.
    let cache_dir = config.data_dir.join("cache");
    std::fs::create_dir_all(&cache_dir).map_err(std::io::Error::other)?;
    let live = Arc::new(live::Live::default());
    // This gateway's own sink, not the process-wide one: `set_sink` gives the first
    // install its way for the life of the process, so a second gateway in one process
    // (`cargo test`, an embedder) would otherwise emit its turns and terminals into
    // the first one's sockets and no client would ever see them (PAN-110).
    let run = crate::run_ctx::RunContext::new(
        sink.clone(),
        cache_dir,
        config.data_dir.clone(),
        state.ui.clone(),
        Arc::new(live::GatewaySurface::new(live.clone(), sink.clone())),
    );
    let state = Arc::new(state);
    // Auto-deny: a question with no client attached is denied at its deadline. The
    // desktop never starts this — a window is an attached client — so its behaviour
    // is unchanged by construction rather than by a special case.
    let watchdog = match config.approval_ttl {
        Some(ttl) => approvals::watch(state.clone(), sink.clone(), Some(ttl)),
        None => {
            eprintln!(
                "approval auto-deny is disabled (--approval-ttl 0): a question with no client attached waits until the turn ends"
            );
            None
        }
    };
    // A shell nobody is watching is closed after `--terminal-idle`, so a tab that was
    // dropped without a close does not leak a PTY for the life of the gateway.
    let watch = terminals::Watch::new(config.terminal_idle);
    // Whatever was uploaded before this gateway last stopped is swept before it
    // serves, so an unreferenced file never outlives one boot cycle. A sweep that
    // cannot read its own directory is a warning, not a reason to refuse to start.
    let uploads_root = crate::uploads::root(&config.data_dir);
    match state.store.lock() {
        Ok(store) => {
            if let Err(error) = crate::uploads::sweep(&uploads_root, &store, crate::storage::now()) {
                eprintln!("upload sweep at boot: {error}");
            }
        }
        Err(error) => eprintln!("upload sweep at boot: {error}"),
    }
    let router = server::router(server::RouterState {
        token: config.token.clone(),
        allowed_origins: config.allowed_origins.clone(),
        static_dir: config.static_dir.clone(),
        events: sink.clone(),
        ctx: dispatch::Ctx {
            state: state.clone(),
            run,
            live,
            watch: watch.clone(),
        },
        uploads: uploads_root,
        upload_max: config.upload_max,
        watch: watch.clone(),
    });
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let addr = listener.local_addr()?;
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // The hourly pass keeps a long-lived gateway honest: an upload whose task was
    // deleted by another writer, or one that simply outlived its cap, goes without
    // a restart. The first tick is skipped because the boot sweep just ran.
    let sweeper = {
        let state = state.clone();
        let data_dir = config.data_dir.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let state = state.clone();
                let root = crate::uploads::root(&data_dir);
                let swept = tokio::task::spawn_blocking(move || {
                    let store = state.store.lock().map_err(|error| error.to_string())?;
                    crate::uploads::sweep(&root, &store, crate::storage::now())
                })
                .await;
                match swept {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => eprintln!("upload sweep: {error}"),
                    Err(error) => eprintln!("upload sweep did not finish: {error}"),
                }
            }
        })
    };
    // A shell nobody is watching is a process, a PTY and a descriptor per tab. The
    // tick is a quarter of the window (capped at a minute), so a close lands close
    // to the deadline rather than up to a window late (PAN-110).
    let terminal_sweeper = (!config.terminal_idle.is_zero()).then(|| {
        let state = state.clone();
        let sink = sink.clone();
        let idle = watch.window();
        let watch = watch.clone();
        let period = (idle / 4).clamp(std::time::Duration::from_secs(1), std::time::Duration::from_secs(60));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                for (id, opened_at) in watch.unattached() {
                    let last_output = sink.last_terminal_output(&id);
                    if !terminals::is_idle(idle, opened_at, last_output, crate::storage::now()) {
                        continue;
                    }
                    // Closing kills the shell's process group; the reader's own exit
                    // frame carries the reason, so the client is told why. Said out
                    // loud too: killing a person's shell deserves a line in the log.
                    eprintln!(
                        "closing terminal {id}: no client attached and no output for {}s",
                        idle.as_secs()
                    );
                    watch.unwatch(&id);
                    if let Err(error) = crate::terminal::close_because(&state.terminals, &id, "idle") {
                        eprintln!("closing idle terminal {id}: {error}");
                    }
                }
            }
        })
    });
    let join = tokio::spawn(async move {
        // The lock lives as long as the server does, and no longer.
        let _lock = lock;
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
        sink,
        watchdog,
        sweeper,
        terminal_sweeper,
        data_dir: config.data_dir,
        state,
    })
}
